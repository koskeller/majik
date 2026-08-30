//! SQLite persistence for a library (`<library>/.majik/library.db`). Files stay on disk as assets;
//! this holds their metadata, the generations that reference them, favorites, albums and generation
//! state.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::{Path, PathBuf};

use crate::model::{Album, AlbumId, Asset, AssetId, GenerationJob, JobId, JobStatus, JobTrace, GenerationId, GenerationInput, Generation, MediaType, Status, ToolId, TraceLabel};
use crate::thumbnails::thumb_key_for_path;

/// Bumped whenever the DDL below changes. Pre-release there is no migration path: a database of
/// another version is recreated from scratch (see [`Db::open`]).
const SCHEMA_VERSION: i64 = 6;

/// The whole schema, in one place. A `generations` row is a generation (its request and its inputs)
/// mirroring its active attempt; `generation_jobs` is one row per provider attempt, with the
/// handle, the outcome and what the provider said; `generation_job_traces` is every HTTP exchange
/// of an attempt. Deleting an asset detaches it; deleting a generation (never done as a hard
/// delete) would take its attempts with it.
const SCHEMA: &str = r#"
CREATE TABLE assets (
    id TEXT PRIMARY KEY,
    content_hash TEXT,
    kind TEXT NOT NULL,
    content_type TEXT NOT NULL,
    file_name TEXT NOT NULL,
    width INTEGER,
    height INTEGER,
    file_size INTEGER,
    duration REAL,
    created_at INTEGER NOT NULL,
    thumbnail TEXT,
    attributes_json TEXT
);
CREATE INDEX assets_created ON assets(created_at DESC);
CREATE INDEX assets_hash ON assets(content_hash);

CREATE TABLE generations (
    id TEXT PRIMARY KEY,
    media_type TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    is_upscaled INTEGER NOT NULL DEFAULT 0,
    request_json TEXT,
    model_name TEXT,
    provider TEXT,
    tool TEXT,
    output_asset_id TEXT REFERENCES assets(id) ON DELETE SET NULL,
    active_job_id TEXT REFERENCES generation_jobs(id) ON DELETE SET NULL,
    deleted_at INTEGER
);
CREATE INDEX generations_created ON generations(created_at DESC);
CREATE INDEX generations_status ON generations(status);
CREATE INDEX generations_media_type ON generations(media_type);
CREATE INDEX generations_favorite ON generations(is_favorite);
CREATE INDEX generations_tool ON generations(tool);
CREATE INDEX generations_output ON generations(output_asset_id);

CREATE TABLE generation_inputs (
    generation_id TEXT NOT NULL REFERENCES generations(id) ON DELETE CASCADE,
    asset_id TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (generation_id, asset_id, role)
);
CREATE INDEX generation_inputs_asset ON generation_inputs(asset_id);

CREATE TABLE generation_jobs (
    id TEXT PRIMARY KEY,
    generation_id TEXT NOT NULL REFERENCES generations(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    status TEXT NOT NULL,
    external_id TEXT,
    poll_url TEXT,
    output_asset_id TEXT REFERENCES assets(id) ON DELETE SET NULL,
    error TEXT,
    error_kind TEXT,
    provider_request_json TEXT,
    provider_create_response_json TEXT,
    provider_final_response_json TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    UNIQUE (generation_id, attempt)
);
CREATE INDEX generation_jobs_media ON generation_jobs(generation_id, created_at);
CREATE INDEX generation_jobs_status ON generation_jobs(status);

CREATE TABLE generation_job_traces (
    job_id TEXT NOT NULL REFERENCES generation_jobs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    at_ms INTEGER NOT NULL,
    label TEXT NOT NULL,
    method TEXT NOT NULL,
    url TEXT NOT NULL,
    status INTEGER,
    duration_ms INTEGER NOT NULL,
    request_body TEXT,
    response_body TEXT,
    error TEXT,
    PRIMARY KEY (job_id, seq)
);

CREATE TABLE albums (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    position INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE album_entries (
    album_id TEXT NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    generation_id TEXT NOT NULL REFERENCES generations(id) ON DELETE CASCADE,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (album_id, generation_id)
);
CREATE INDEX album_entries_media ON album_entries(generation_id);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (or create) the database at `path`. A database written by another schema version is
    /// removed and created afresh: the app isn't released yet, so there is no migration history to
    /// keep. The files in the folder stay; only the metadata about them goes.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            let version = Self::connect(path)?.user_version()?;
            if version != 0 && version != SCHEMA_VERSION {
                tracing::warn!(target: "majik", "{} is schema v{version}, this build wants v{SCHEMA_VERSION}: recreating it (pre-release, no migrations)", path.display());
                for suffix in ["", "-wal", "-shm"] {
                    let file = PathBuf::from(format!("{}{suffix}", path.display()));
                    if file.exists() {
                        std::fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
                    }
                }
            }
        }
        let db = Self::connect(path)?;
        db.create_schema()?;
        Ok(db)
    }

    fn connect(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self { conn };
        db.create_schema()?;
        Ok(db)
    }

    fn user_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Create the schema in an empty database; one transaction, so a failure halfway leaves
    /// nothing behind. A database of the current version is left as it is.
    fn create_schema(&self) -> Result<()> {
        let version = self.user_version()?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }
        if version != 0 {
            return Err(anyhow!("database is schema v{version}, expected v{SCHEMA_VERSION}"));
        }
        self.conn.execute_batch("BEGIN")?;
        let result = self.conn.execute_batch(SCHEMA).map_err(Into::into).and_then(|()| Ok(self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?));
        match result {
            Ok(()) => Ok(self.conn.execute_batch("COMMIT")?),
            Err(e) => {
                if let Err(rollback) = self.conn.execute_batch("ROLLBACK") {
                    tracing::warn!(target: "majik", "rolling back schema creation: {rollback:#}");
                }
                Err(e)
            }
        }
    }

    // ----- generations -----------------------------------------------------------------

    /// Live (not deleted) generations.
    pub fn generation_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM generations WHERE deleted_at IS NULL", [], |r| r.get(0))?)
    }

    /// All live rows, newest first. The file fields (`path`, dimensions, size, thumbnail) are left
    /// empty here: they belong to the output asset, which the library copies in once it has loaded
    /// the assets.
    /// All live rows, newest first, with the error and the provider handle of each one's active
    /// attempt projected onto it (the handle only while that attempt is still in flight). The
    /// file fields (`path`, dimensions, size, thumbnail) are left empty here: they belong to the
    /// output asset, which the library copies in once it has loaded the assets.
    pub fn load_generations(&self) -> Result<Vec<Generation>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.media_type, m.status, m.created_at, m.is_favorite, m.is_upscaled, m.request_json, m.model_name, m.provider,
                    m.tool, m.output_asset_id, m.active_job_id,
                    CASE WHEN j.status IN ('queued', 'running') THEN j.external_id END AS job_id,
                    CASE WHEN j.status IN ('queued', 'running') THEN j.poll_url END AS poll_url,
                    j.error, j.error_kind, j.started_at
             FROM generations m LEFT JOIN generation_jobs j ON j.id = m.active_job_id
             WHERE m.deleted_at IS NULL ORDER BY m.created_at DESC, m.id ASC",
        )?;
        let rows = stmt.query_map([], row_to_item)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Write the row's own columns. The attempt-owned fields (`error`, `job_id`, …) are not here:
    /// they are written through the job (`mark_job_running`, `finish_job`) and read back by the
    /// join in [`Db::load_generations`].
    pub fn upsert_generation(&self, item: &Generation) -> Result<()> {
        self.conn.execute(
            r#"INSERT INTO generations (id, media_type, status, created_at, is_favorite, is_upscaled, request_json, model_name, provider,
                tool, output_asset_id, active_job_id)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
               ON CONFLICT(id) DO UPDATE SET
                media_type=excluded.media_type, status=excluded.status, created_at=excluded.created_at,
                is_favorite=excluded.is_favorite, is_upscaled=excluded.is_upscaled, request_json=excluded.request_json,
                model_name=excluded.model_name, provider=excluded.provider, tool=excluded.tool,
                output_asset_id=excluded.output_asset_id, active_job_id=excluded.active_job_id"#,
            params![
                item.id.0,
                media_type_raw(item.media_type),
                status_raw(item.status),
                item.created_at_ms as i64,
                item.is_favorite as i64,
                item.is_upscaled as i64,
                item.request_json,
                item.model_name,
                item.provider,
                item.tool.map(tool_raw),
                item.output_asset_id.as_ref().map(|a| a.0.clone()),
                item.active_job_id.as_ref().map(|j| j.0.clone()),
            ],
        )?;
        Ok(())
    }

    // ----- jobs ------------------------------------------------------------------

    /// The attempt number the next job of `generation` gets (1 for the first).
    pub fn next_attempt(&self, generation: &GenerationId) -> Result<u32> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(attempt), 0) + 1 FROM generation_jobs WHERE generation_id = ?1", params![generation.0], |r| r.get::<_, i64>(0))? as u32)
    }

    pub fn insert_job(&self, job: &GenerationJob) -> Result<()> {
        self.conn.execute(
            "INSERT INTO generation_jobs (id, generation_id, attempt, status, external_id, poll_url, output_asset_id, error, error_kind,
                provider_request_json, provider_create_response_json, provider_final_response_json, created_at, started_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                job.id.0,
                job.generation_id.0,
                job.attempt as i64,
                job_status_raw(job.status),
                job.external_id,
                job.poll_url,
                job.output_asset_id.as_ref().map(|a| a.0.clone()),
                job.error,
                job.error_kind,
                job.provider_request_json,
                job.provider_create_response_json,
                job.provider_final_response_json,
                job.created_at_ms as i64,
                job.started_at_ms.map(|v| v as i64),
                job.finished_at_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn set_active_job(&self, generation: &GenerationId, job: &JobId) -> Result<()> {
        self.conn.execute("UPDATE generations SET active_job_id = ?2 WHERE id = ?1", params![generation.0, job.0])?;
        Ok(())
    }

    /// The provider took the job: it is running under `external_id`, from `now_ms` if this is the
    /// first report of it (the engine's internal retry reports again; the original start stands).
    pub fn mark_job_running(&self, job: &JobId, external_id: Option<&str>, poll_url: Option<&str>, now_ms: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE generation_jobs SET status = 'running', external_id = ?2, poll_url = ?3, started_at = COALESCE(started_at, ?4) WHERE id = ?1",
            params![job.0, external_id, poll_url, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn complete_job(&self, job: &JobId, output: &AssetId, now_ms: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE generation_jobs SET status = 'completed', output_asset_id = ?2, error = NULL, error_kind = NULL,
                started_at = COALESCE(started_at, ?3), finished_at = ?3 WHERE id = ?1",
            params![job.0, output.0, now_ms as i64],
        )?;
        Ok(())
    }

    /// End the attempt as failed or canceled, with what went wrong.
    pub fn finish_job(&self, job: &JobId, status: JobStatus, error: Option<&str>, error_kind: Option<&str>, now_ms: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE generation_jobs SET status = ?2, error = ?3, error_kind = ?4, finished_at = ?5 WHERE id = ?1",
            params![job.0, job_status_raw(status), error, error_kind, now_ms as i64],
        )?;
        Ok(())
    }

    /// Rewrite when an attempt was created (and started, if it had). Only tests use it, to age an
    /// attempt past its deadline.
    pub fn set_job_created_at(&self, job: &JobId, created_at_ms: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE generation_jobs SET created_at = ?2, started_at = CASE WHEN started_at IS NULL THEN NULL ELSE ?2 END WHERE id = ?1",
            params![job.0, created_at_ms as i64],
        )?;
        Ok(())
    }

    pub fn job(&self, id: &JobId) -> Result<Option<GenerationJob>> {
        Ok(self.conn.query_row(&format!("{JOB_COLUMNS} WHERE id = ?1"), params![id.0], row_to_job).optional()?)
    }

    /// Every attempt of a generation, first to last.
    pub fn load_jobs(&self, generation: &GenerationId) -> Result<Vec<GenerationJob>> {
        let mut stmt = self.conn.prepare(&format!("{JOB_COLUMNS} WHERE generation_id = ?1 ORDER BY attempt"))?;
        let rows = stmt.query_map(params![generation.0], row_to_job)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Append one exchange to the job's traces and fold it into the job's provider columns: a
    /// submit sets the request and the create response, and every response seen becomes the final
    /// one (the latest wins, so the engine's retry-once leaves the submit that produced the
    /// outcome). A download records nothing but its size. Returns the entry's sequence number.
    pub fn record_trace(&self, job: &JobId, trace: &JobTrace) -> Result<u32> {
        let seq: i64 = self.conn.query_row("SELECT COALESCE(MAX(seq), -1) + 1 FROM generation_job_traces WHERE job_id = ?1", params![job.0], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO generation_job_traces (job_id, seq, at_ms, label, method, url, status, duration_ms, request_body, response_body, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                job.0,
                seq,
                trace.at_ms as i64,
                trace.label.raw(),
                trace.method,
                trace.url,
                trace.status.map(i64::from),
                trace.duration_ms as i64,
                trace.request_body,
                trace.response_body,
                trace.error,
            ],
        )?;
        match trace.label {
            TraceLabel::Submit => {
                self.conn.execute(
                    "UPDATE generation_jobs SET provider_request_json = ?2, provider_create_response_json = ?3, provider_final_response_json = COALESCE(?3, provider_final_response_json) WHERE id = ?1",
                    params![job.0, trace.request_body, trace.response_body],
                )?;
            }
            TraceLabel::Poll | TraceLabel::Result => {
                if trace.response_body.is_some() {
                    self.conn.execute("UPDATE generation_jobs SET provider_final_response_json = ?2 WHERE id = ?1", params![job.0, trace.response_body])?;
                }
            }
            TraceLabel::Download => {}
        }
        Ok(seq as u32)
    }

    pub fn load_traces(&self, job: &JobId) -> Result<Vec<JobTrace>> {
        let mut stmt = self.conn.prepare(
            "SELECT at_ms, label, method, url, status, duration_ms, request_body, response_body, error FROM generation_job_traces WHERE job_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![job.0], |r| {
            Ok(JobTrace {
                at_ms: r.get::<_, i64>(0)? as u64,
                label: TraceLabel::from_raw(&r.get::<_, String>(1)?).unwrap_or(TraceLabel::Submit),
                method: r.get(2)?,
                url: r.get(3)?,
                status: r.get::<_, Option<i64>>(4)?.map(|v| v as u16),
                duration_ms: r.get::<_, i64>(5)? as u64,
                request_body: r.get(6)?,
                response_body: r.get(7)?,
                error: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Soft delete: the row leaves every feed but keeps its request and its references, and its
    /// assets are untouched. Album entries go, so the album counts stay correct.
    pub fn soft_delete_generation(&self, id: &GenerationId, now_ms: u64) -> Result<()> {
        self.conn.execute("UPDATE generations SET deleted_at = ?2 WHERE id = ?1", params![id.0, now_ms as i64])?;
        self.conn.execute("DELETE FROM album_entries WHERE generation_id = ?1", params![id.0])?;
        Ok(())
    }

    pub fn set_favorite(&self, id: &GenerationId, favorite: bool) -> Result<()> {
        self.conn.execute("UPDATE generations SET is_favorite = ?2 WHERE id = ?1", params![id.0, favorite as i64])?;
        Ok(())
    }

    // ----- assets ----------------------------------------------------------------

    /// All assets, newest first. `root` resolves the file and thumbnail keys to absolute paths;
    /// `missing` is left `false` for the library to reconcile against the folder.
    pub fn load_assets(&self, root: &Path) -> Result<Vec<Asset>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content_hash, kind, content_type, file_name, width, height, file_size, duration, created_at, thumbnail FROM assets ORDER BY created_at DESC, id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Asset {
                id: AssetId(r.get(0)?),
                content_hash: r.get(1)?,
                kind: media_type_from_raw(&r.get::<_, String>(2)?),
                content_type: r.get(3)?,
                path: root.join(r.get::<_, String>(4)?),
                width: r.get(5)?,
                height: r.get(6)?,
                file_size: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                duration_secs: r.get(8)?,
                created_at_ms: r.get::<_, i64>(9)? as u64,
                thumbnail: r.get::<_, Option<String>>(10)?.map(|key| root.join(key)),
                missing: false,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// `file_key` is the blob key of the file (relative to the root).
    pub fn insert_asset(&self, asset: &Asset, file_key: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO assets (id, content_hash, kind, content_type, file_name, width, height, file_size, duration, created_at, thumbnail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                asset.id.0,
                asset.content_hash,
                media_type_raw(asset.kind),
                asset.content_type,
                file_key,
                asset.width,
                asset.height,
                asset.file_size.map(|v| v as i64),
                asset.duration_secs,
                asset.created_at_ms as i64,
                asset.thumbnail.as_deref().and_then(thumb_key_for_path),
            ],
        )?;
        Ok(())
    }

    /// Rewrite everything about an asset's bytes: a regenerated output goes into the same file.
    pub fn update_asset_file(&self, asset: &Asset) -> Result<()> {
        self.conn.execute(
            "UPDATE assets SET content_hash = ?2, content_type = ?3, width = ?4, height = ?5, file_size = ?6, duration = ?7, thumbnail = ?8 WHERE id = ?1",
            params![
                asset.id.0,
                asset.content_hash,
                asset.content_type,
                asset.width,
                asset.height,
                asset.file_size.map(|v| v as i64),
                asset.duration_secs,
                asset.thumbnail.as_deref().and_then(thumb_key_for_path),
            ],
        )?;
        Ok(())
    }

    pub fn set_asset_info(&self, id: &AssetId, width: Option<u32>, height: Option<u32>, file_size: Option<u64>, duration_secs: Option<f64>) -> Result<()> {
        self.conn.execute(
            "UPDATE assets SET width = COALESCE(?2, width), height = COALESCE(?3, height), file_size = COALESCE(?4, file_size), duration = COALESCE(?5, duration) WHERE id = ?1",
            params![id.0, width, height, file_size.map(|v| v as i64), duration_secs],
        )?;
        Ok(())
    }

    /// `thumb` is the local path of the stored thumbnail; only its blob key is persisted.
    pub fn set_asset_thumbnail(&self, id: &AssetId, thumb: Option<&Path>) -> Result<()> {
        self.conn.execute("UPDATE assets SET thumbnail = ?2 WHERE id = ?1", params![id.0, thumb.and_then(thumb_key_for_path)])?;
        Ok(())
    }

    pub fn delete_asset(&self, id: &AssetId) -> Result<()> {
        self.conn.execute("DELETE FROM assets WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    pub fn find_asset_by_hash(&self, hash: &str) -> Result<Option<AssetId>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM assets WHERE content_hash = ?1 ORDER BY created_at LIMIT 1", params![hash], |r| r.get::<_, String>(0))
            .optional()?
            .map(AssetId))
    }

    // ----- inputs ----------------------------------------------------------------

    /// Every generation → asset link, including those of deleted generations (an asset they used
    /// stays referenced only by live ones; the library filters).
    pub fn load_inputs(&self) -> Result<Vec<GenerationInput>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.generation_id, i.asset_id, i.role, i.position FROM generation_inputs i JOIN generations m ON m.id = i.generation_id WHERE m.deleted_at IS NULL ORDER BY i.generation_id, i.role, i.position",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(GenerationInput { generation_id: GenerationId(r.get(0)?), asset_id: AssetId(r.get(1)?), role: r.get(2)?, position: r.get::<_, i64>(3)? as usize })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn insert_input(&self, input: &GenerationInput) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO generation_inputs (generation_id, asset_id, role, position) VALUES (?1, ?2, ?3, ?4)",
            params![input.generation_id.0, input.asset_id.0, input.role, input.position as i64],
        )?;
        Ok(())
    }

    // ----- albums ----------------------------------------------------------------

    pub fn load_albums(&self) -> Result<Vec<Album>> {
        let mut stmt = self.conn.prepare("SELECT id, name, created_at FROM albums ORDER BY position, created_at, id")?;
        let mut albums: Vec<Album> = stmt
            .query_map([], |r| Ok(Album { id: AlbumId(r.get(0)?), name: r.get(1)?, created_at_ms: r.get::<_, i64>(2)? as u64, items: Vec::new() }))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut entries = self.conn.prepare("SELECT album_id, generation_id FROM album_entries ORDER BY added_at, generation_id")?;
        for row in entries.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (album_id, generation_id) = row?;
            if let Some(a) = albums.iter_mut().find(|a| a.id.0 == album_id) {
                a.items.push(GenerationId(generation_id));
            }
        }
        Ok(albums)
    }

    pub fn insert_album(&self, album: &Album) -> Result<()> {
        let position: i64 = self.conn.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM albums", [], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO albums (id, name, created_at, position) VALUES (?1, ?2, ?3, ?4)",
            params![album.id.0, album.name, album.created_at_ms as i64, position],
        )?;
        for (i, m) in album.items.iter().enumerate() {
            self.conn.execute("INSERT OR IGNORE INTO album_entries (album_id, generation_id, added_at) VALUES (?1, ?2, ?3)", params![album.id.0, m.0, i as i64])?;
        }
        Ok(())
    }

    pub fn rename_album(&self, id: &AlbumId, name: &str) -> Result<()> {
        self.conn.execute("UPDATE albums SET name = ?2 WHERE id = ?1", params![id.0, name])?;
        Ok(())
    }

    pub fn delete_album(&self, id: &AlbumId) -> Result<()> {
        self.conn.execute("DELETE FROM albums WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    pub fn add_to_album(&self, album: &AlbumId, ids: &[GenerationId], now_ms: u64) -> Result<()> {
        for (i, id) in ids.iter().enumerate() {
            self.conn.execute(
                "INSERT OR IGNORE INTO album_entries (album_id, generation_id, added_at) VALUES (?1, ?2, ?3)",
                params![album.0, id.0, now_ms as i64 + i as i64],
            )?;
        }
        Ok(())
    }

    pub fn remove_from_album(&self, album: &AlbumId, ids: &[GenerationId]) -> Result<()> {
        for id in ids {
            self.conn.execute("DELETE FROM album_entries WHERE album_id = ?1 AND generation_id = ?2", params![album.0, id.0])?;
        }
        Ok(())
    }

    // ----- settings --------------------------------------------------------------

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Refuse every write on this connection (SQLite's `query_only`). Only tests use it, to see
    /// what a failed write leaves behind.
    pub fn set_read_only(&self, read_only: bool) -> Result<()> {
        self.conn.execute_batch(if read_only { "PRAGMA query_only = 1" } else { "PRAGMA query_only = 0" })?;
        Ok(())
    }

    pub fn transaction<T>(&mut self, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN")?;
        // Catch a panic in `f` so the connection isn't left inside an open transaction.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        match result {
            Ok(Ok(v)) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Ok(Err(e)) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
            Err(panic) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                std::panic::resume_unwind(panic);
            }
        }
    }
}

/// Blob-key prefix of imported and input asset files (outputs live in the root as `<uuid>.<ext>`).
pub const ASSETS_PREFIX: &str = ".majik/assets";

/// MIME type for a stored file, from its extension, else the type's default.
pub fn content_type_for_file(file_name: &str, kind: MediaType) -> &'static str {
    let ext = file_name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => match kind {
            MediaType::Image => "image/png",
            MediaType::Video => "video/mp4",
            MediaType::Audio => "audio/mpeg",
        },
    }
}

/// File extension for a MIME type or legacy UTI.
pub fn extension_for_content_type(content_type: &str) -> &'static str {
    match content_type.to_ascii_lowercase().as_str() {
        "image/png" | "public.png" => "png",
        "image/jpeg" | "public.jpeg" => "jpg",
        "image/webp" | "public.webp" | "org.webmproject.webp" => "webp",
        "image/gif" | "com.compuserve.gif" => "gif",
        "video/mp4" | "public.mpeg-4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" | "public.mp3" => "mp3",
        "audio/wav" | "audio/x-wav" | "public.wav" | "com.microsoft.waveform-audio" => "wav",
        _ => "bin",
    }
}

fn row_to_item(r: &Row<'_>) -> rusqlite::Result<Generation> {
    let media_type = media_type_from_raw(&r.get::<_, String>("media_type")?);
    let status = status_from_raw(&r.get::<_, String>("status")?);
    Ok(Generation {
        id: GenerationId(r.get("id")?),
        path: None,
        media_type,
        status,
        created_at_ms: r.get::<_, i64>("created_at")? as u64,
        width: None,
        height: None,
        duration_secs: None,
        file_size: None,
        is_favorite: r.get::<_, i64>("is_favorite")? != 0,
        is_upscaled: r.get::<_, i64>("is_upscaled")? != 0,
        thumbnail: None,
        output_asset_id: r.get::<_, Option<String>>("output_asset_id")?.map(AssetId),
        request_json: r.get("request_json")?,
        model_name: r.get("model_name")?,
        provider: r.get("provider")?,
        error: r.get("error")?,
        error_kind: r.get("error_kind")?,
        tool: r.get::<_, Option<String>>("tool")?.as_deref().and_then(tool_from_raw),
        job_id: r.get("job_id")?,
        poll_url: r.get("poll_url")?,
        started_at_ms: r.get::<_, Option<i64>>("started_at")?.map(|v| v as u64),
        active_job_id: r.get::<_, Option<String>>("active_job_id")?.map(JobId),
    })
}

const JOB_COLUMNS: &str = "SELECT id, generation_id, attempt, status, external_id, poll_url, output_asset_id, error, error_kind,
    provider_request_json, provider_create_response_json, provider_final_response_json, created_at, started_at, finished_at FROM generation_jobs";

fn row_to_job(r: &Row<'_>) -> rusqlite::Result<GenerationJob> {
    Ok(GenerationJob {
        id: JobId(r.get(0)?),
        generation_id: GenerationId(r.get(1)?),
        attempt: r.get::<_, i64>(2)? as u32,
        status: job_status_from_raw(&r.get::<_, String>(3)?),
        external_id: r.get(4)?,
        poll_url: r.get(5)?,
        output_asset_id: r.get::<_, Option<String>>(6)?.map(AssetId),
        error: r.get(7)?,
        error_kind: r.get(8)?,
        provider_request_json: r.get(9)?,
        provider_create_response_json: r.get(10)?,
        provider_final_response_json: r.get(11)?,
        created_at_ms: r.get::<_, i64>(12)? as u64,
        started_at_ms: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        finished_at_ms: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
    })
}

pub fn job_status_raw(s: JobStatus) -> &'static str {
    match s {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Canceled => "canceled",
    }
}

pub fn job_status_from_raw(s: &str) -> JobStatus {
    match s {
        "queued" => JobStatus::Queued,
        "running" => JobStatus::Running,
        "failed" => JobStatus::Failed,
        "canceled" => JobStatus::Canceled,
        _ => JobStatus::Completed,
    }
}

pub fn media_type_raw(t: MediaType) -> &'static str {
    match t {
        MediaType::Image => "image",
        MediaType::Video => "video",
        MediaType::Audio => "audio",
    }
}

pub fn media_type_from_raw(s: &str) -> MediaType {
    match s {
        "video" => MediaType::Video,
        "audio" => MediaType::Audio,
        _ => MediaType::Image,
    }
}

pub fn status_raw(s: Status) -> &'static str {
    match s {
        Status::Generating => "generating",
        // Missing is derived from the folder on open; the row itself is a completed one.
        Status::Completed | Status::Missing => "completed",
        Status::Failed => "failed",
    }
}

pub fn status_from_raw(s: &str) -> Status {
    match s {
        "generating" => Status::Generating,
        "failed" => Status::Failed,
        _ => Status::Completed,
    }
}

pub fn tool_raw(t: ToolId) -> &'static str {
    match t {
        ToolId::Upscale => "upscale",
        ToolId::RemoveBackground => "removeBackground",
    }
}

pub fn tool_from_raw(s: &str) -> Option<ToolId> {
    match s {
        "upscale" => Some(ToolId::Upscale),
        "removeBackground" => Some(ToolId::RemoveBackground),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn item(id: &str) -> Generation {
        Generation {
            id: GenerationId(id.into()),
            path: None,
            media_type: MediaType::Image,
            status: Status::Completed,
            created_at_ms: 42,
            width: None,
            height: None,
            duration_secs: None,
            file_size: None,
            is_favorite: false,
            is_upscaled: false,
            thumbnail: None,
            output_asset_id: None,
            request_json: None,
            model_name: Some("m".into()),
            provider: None,
            error: None,
            error_kind: None,
            tool: None,
            job_id: None,
            poll_url: None,
            started_at_ms: None,
            active_job_id: None,
        }
    }

    fn job(id: &str, generations: &str, attempt: u32) -> GenerationJob {
        GenerationJob {
            id: JobId(id.into()),
            generation_id: GenerationId(generations.into()),
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
            created_at_ms: 1,
            started_at_ms: None,
            finished_at_ms: None,
        }
    }

    fn trace(label: TraceLabel, request: Option<&str>, response: Option<&str>) -> JobTrace {
        JobTrace {
            at_ms: 5,
            label,
            method: "POST".into(),
            url: "https://provider.example/run".into(),
            status: Some(200),
            duration_ms: 12,
            request_body: request.map(str::to_string),
            response_body: response.map(str::to_string),
            error: None,
        }
    }

    fn asset(id: &str, created_at_ms: u64) -> Asset {
        Asset {
            id: AssetId(id.into()),
            content_hash: Some(format!("hash-{id}")),
            kind: MediaType::Image,
            content_type: "image/png".into(),
            path: PathBuf::from(format!("/tmp/{id}.png")),
            width: Some(4),
            height: Some(3),
            file_size: Some(10),
            duration_secs: None,
            created_at_ms,
            thumbnail: None,
            missing: false,
        }
    }

    #[test]
    fn generation_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_generation(&item("a")).unwrap();
        db.set_favorite(&GenerationId("a".into()), true).unwrap();
        let rows = db.load_generations().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_favorite);
        db.soft_delete_generation(&GenerationId("a".into()), 7).unwrap();
        assert_eq!(db.generation_count().unwrap(), 0);
        assert!(db.load_generations().unwrap().is_empty(), "a deleted row is out of every listing");
        let kept: i64 = db.conn.query_row("SELECT COUNT(*) FROM generations WHERE deleted_at = 7", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1, "… but the row itself stays");
    }

    #[test]
    fn missing_status_is_stored_as_completed() {
        let db = Db::open_in_memory().unwrap();
        let mut missing = item("a");
        missing.status = Status::Missing;
        missing.output_asset_id = Some(AssetId("x".into()));
        db.insert_asset(&asset("x", 1), "a.png").unwrap();
        db.upsert_generation(&missing).unwrap();
        let rows = db.load_generations().unwrap();
        assert_eq!(rows[0].status, Status::Completed);
        assert_eq!(rows[0].output_asset_id, Some(AssetId("x".into())), "the output reference survives");
    }

    #[test]
    fn assets_and_inputs_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.insert_asset(&asset("in", 1), ".majik/assets/hash-in.png").unwrap();
        db.insert_asset(&asset("out", 2), "m.png").unwrap();
        let mut m = item("m");
        m.output_asset_id = Some(AssetId("out".into()));
        db.upsert_generation(&m).unwrap();
        db.insert_input(&GenerationInput { generation_id: GenerationId("m".into()), asset_id: AssetId("in".into()), role: "reference_image".into(), position: 0 }).unwrap();

        let assets = db.load_assets(Path::new("/root")).unwrap();
        assert_eq!(assets.iter().map(|a| a.id.0.as_str()).collect::<Vec<_>>(), ["out", "in"], "newest first");
        assert_eq!(assets[1].path, Path::new("/root/.majik/assets/hash-in.png"));
        assert_eq!(db.find_asset_by_hash("hash-in").unwrap(), Some(AssetId("in".into())));
        assert_eq!(db.load_inputs().unwrap().len(), 1);

        db.set_asset_info(&AssetId("out".into()), Some(8), None, None, Some(1.5)).unwrap();
        db.set_asset_thumbnail(&AssetId("out".into()), Some(Path::new("/root/.majik/thumbs/t.jpg"))).unwrap();
        let out = db.load_assets(Path::new("/root")).unwrap().into_iter().find(|a| a.id.0 == "out").unwrap();
        assert_eq!((out.width, out.height, out.duration_secs), (Some(8), Some(3), Some(1.5)), "COALESCE keeps what wasn't given");
        assert_eq!(out.thumbnail.as_deref(), Some(Path::new("/root/.majik/thumbs/t.jpg")));

        // A deleted generation's inputs no longer count.
        db.soft_delete_generation(&GenerationId("m".into()), 9).unwrap();
        assert!(db.load_inputs().unwrap().is_empty());
        // Deleting the output asset detaches it from the row; deleting an input drops its links.
        db.delete_asset(&AssetId("out".into())).unwrap();
        let output: Option<String> = db.conn.query_row("SELECT output_asset_id FROM generations WHERE id = 'm'", [], |r| r.get(0)).unwrap();
        assert!(output.is_none());
        db.delete_asset(&AssetId("in".into())).unwrap();
        let links: i64 = db.conn.query_row("SELECT COUNT(*) FROM generation_inputs", [], |r| r.get(0)).unwrap();
        assert_eq!(links, 0);
    }

    #[test]
    fn thumbnails_are_stored_relative_to_the_root() {
        let db = Db::open_in_memory().unwrap();
        let mut a = asset("a", 1);
        a.thumbnail = Some(PathBuf::from("/old/root/.majik/thumbs/deadbeef.jpg"));
        db.insert_asset(&a, "a.png").unwrap();
        db.insert_asset(&asset("b", 2), "b.png").unwrap();
        db.set_asset_thumbnail(&AssetId("b".into()), Some(Path::new("/old/root/.majik/thumbs/cafe.png"))).unwrap();
        let rows = db.load_assets(Path::new("/new")).unwrap();
        let by_id = |id: &str| rows.iter().find(|r| r.id.0 == id).unwrap().thumbnail.clone();
        assert_eq!(by_id("a").as_deref(), Some(Path::new("/new/.majik/thumbs/deadbeef.jpg")));
        assert_eq!(by_id("b").as_deref(), Some(Path::new("/new/.majik/thumbs/cafe.png")));
    }

    #[test]
    fn fresh_database_gets_the_current_schema() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
        let tables: Vec<String> = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(tables, ["album_entries", "albums", "assets", "generation_inputs", "generation_job_traces", "generation_jobs", "generations", "settings"]);
        let foreign_keys: i64 = db.conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(foreign_keys, 1);
        db.create_schema().unwrap();
        assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION, "opening again is a no-op");
    }

    #[test]
    fn an_older_database_is_wiped_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let db = Db::open(&path).unwrap();
            db.upsert_generation(&item("kept-if-migrated")).unwrap();
            db.conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1).unwrap();
        }
        assert!(path.with_extension("db-wal").exists() || path.exists());
        let db = Db::open(&path).unwrap();
        assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
        assert!(db.load_generations().unwrap().is_empty(), "no migration: the old rows are gone");
        drop(db);
        let db = Db::open(&path).unwrap();
        db.upsert_generation(&item("new")).unwrap();
        drop(db);
        assert_eq!(Db::open(&path).unwrap().load_generations().unwrap().len(), 1, "a current database is kept");
    }

    #[test]
    fn jobs_round_trip_and_project_onto_the_row() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_generation(&item("m")).unwrap();
        assert_eq!(db.next_attempt(&GenerationId("m".into())).unwrap(), 1);
        db.insert_job(&job("j1", "m", 1)).unwrap();
        db.set_active_job(&GenerationId("m".into()), &JobId("j1".into())).unwrap();
        db.mark_job_running(&JobId("j1".into()), Some("ext-1"), Some("https://poll"), 10).unwrap();
        db.mark_job_running(&JobId("j1".into()), Some("ext-1"), Some("https://poll"), 20).unwrap();
        let row = &db.load_generations().unwrap()[0];
        assert_eq!(row.active_job_id, Some(JobId("j1".into())));
        assert_eq!((row.job_id.as_deref(), row.poll_url.as_deref()), (Some("ext-1"), Some("https://poll")), "the handle of a running attempt is the row's");
        let running = db.job(&JobId("j1".into())).unwrap().unwrap();
        assert_eq!((running.status, running.started_at_ms), (JobStatus::Running, Some(10)), "the first start stands");
        assert_eq!(row.started_at_ms, Some(10), "… and is the row's clock");

        db.finish_job(&JobId("j1".into()), JobStatus::Failed, Some("boom"), Some("server_error"), 30).unwrap();
        let row = &db.load_generations().unwrap()[0];
        assert_eq!((row.error.as_deref(), row.error_kind.as_deref()), (Some("boom"), Some("server_error")), "the attempt's error is the row's");
        assert!(row.job_id.is_none() && row.poll_url.is_none(), "a spent handle isn't offered for resume");
        assert_eq!(db.job(&JobId("j1".into())).unwrap().unwrap().external_id.as_deref(), Some("ext-1"), "… but the job keeps it");

        assert_eq!(db.next_attempt(&GenerationId("m".into())).unwrap(), 2);
        db.insert_job(&job("j2", "m", 2)).unwrap();
        db.set_active_job(&GenerationId("m".into()), &JobId("j2".into())).unwrap();
        db.insert_asset(&asset("out", 3), "m.png").unwrap();
        db.complete_job(&JobId("j2".into()), &AssetId("out".into()), 40).unwrap();
        let jobs = db.load_jobs(&GenerationId("m".into())).unwrap();
        assert_eq!(jobs.iter().map(|j| (j.attempt, j.status)).collect::<Vec<_>>(), [(1, JobStatus::Failed), (2, JobStatus::Completed)]);
        assert_eq!((jobs[1].output_asset_id.as_ref().map(|a| a.0.as_str()), jobs[1].started_at_ms, jobs[1].finished_at_ms), (Some("out"), Some(40), Some(40)));
        assert!(db.load_generations().unwrap()[0].error.is_none(), "the new attempt has no error");

        db.delete_asset(&AssetId("out".into())).unwrap();
        assert!(db.job(&JobId("j2".into())).unwrap().unwrap().output_asset_id.is_none(), "deleting the asset detaches it from the attempt");
        db.soft_delete_generation(&GenerationId("m".into()), 50).unwrap();
        assert_eq!(db.load_jobs(&GenerationId("m".into())).unwrap().len(), 2, "a soft-deleted generation keeps its history");
    }

    #[test]
    fn traces_are_sequenced_per_job_and_folded_into_its_provider_columns() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_generation(&item("m")).unwrap();
        db.insert_job(&job("j", "m", 1)).unwrap();
        db.insert_job(&job("k", "m", 2)).unwrap();
        let j = JobId("j".into());
        assert_eq!(db.record_trace(&j, &trace(TraceLabel::Submit, Some(r#"{"prompt":"a"}"#), Some(r#"{"request_id":"1"}"#))).unwrap(), 0);
        assert_eq!(db.record_trace(&j, &trace(TraceLabel::Poll, None, Some(r#"{"status":"IN_QUEUE"}"#))).unwrap(), 1);
        assert_eq!(db.record_trace(&JobId("k".into()), &trace(TraceLabel::Submit, None, None)).unwrap(), 0, "sequences are per job");
        assert_eq!(db.record_trace(&j, &trace(TraceLabel::Result, None, Some(r#"{"images":[]}"#))).unwrap(), 2);
        assert_eq!(db.record_trace(&j, &trace(TraceLabel::Download, None, Some("123 bytes"))).unwrap(), 3);

        let job = db.job(&j).unwrap().unwrap();
        assert_eq!(job.provider_request_json.as_deref(), Some(r#"{"prompt":"a"}"#));
        assert_eq!(job.provider_create_response_json.as_deref(), Some(r#"{"request_id":"1"}"#));
        assert_eq!(job.provider_final_response_json.as_deref(), Some(r#"{"images":[]}"#), "the last result body; a download changes nothing");

        // The engine's retry-once submits again: the submit that produced the outcome wins.
        db.record_trace(&j, &trace(TraceLabel::Submit, Some(r#"{"prompt":"a","retry":1}"#), Some(r#"{"request_id":"2"}"#))).unwrap();
        let job = db.job(&j).unwrap().unwrap();
        assert_eq!(job.provider_request_json.as_deref(), Some(r#"{"prompt":"a","retry":1}"#));
        assert_eq!(job.provider_final_response_json.as_deref(), Some(r#"{"request_id":"2"}"#));

        let traces = db.load_traces(&j).unwrap();
        assert_eq!(traces.iter().map(|t| t.label).collect::<Vec<_>>(), [TraceLabel::Submit, TraceLabel::Poll, TraceLabel::Result, TraceLabel::Download, TraceLabel::Submit]);
        assert_eq!(traces[0], trace(TraceLabel::Submit, Some(r#"{"prompt":"a"}"#), Some(r#"{"request_id":"1"}"#)));
    }

    #[test]
    fn albums_cascade() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_generation(&item("a")).unwrap();
        let album = Album { id: AlbumId("al".into()), name: "Trip".into(), created_at_ms: 1, items: vec![] };
        db.insert_album(&album).unwrap();
        db.add_to_album(&album.id, &[GenerationId("a".into())], 5).unwrap();
        assert_eq!(db.load_albums().unwrap()[0].items.len(), 1);
        db.soft_delete_generation(&GenerationId("a".into()), 6).unwrap();
        assert_eq!(db.load_albums().unwrap()[0].items.len(), 0);
        db.delete_album(&album.id).unwrap();
        assert!(db.load_albums().unwrap().is_empty());
    }
}
