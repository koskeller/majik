//! Fills a library with generated content, so the app can be exercised at the size a real one
//! reaches. No provider and no engine are involved: the rows, the attempts and the files are
//! written the way [`majik_core::Library`] writes them, in batched transactions, with the media
//! rendered locally (`majik_core::images` / `majik_core::video` / [`tone_wav`]).
//!
//! What it produces is meant to *cost* what a real library costs: files in the size range a
//! provider returns, a spread of creation dates, every status the feed can show (completed, failed,
//! missing file, still generating), favorites, albums, tool rows with their input asset, imported
//! assets no generation owns, and a stored `Request` on every row so Recreate works.
//!
//! ```no_run
//! use majik_generation::seed::{seed_library, SeedOptions};
//! let report = seed_library(&SeedOptions { images: 9_000, videos: 800, ..SeedOptions::at("/tmp/perf") })?;
//! println!("{} generations", report.generations);
//! # Ok::<(), anyhow::Error>(())
//! ```

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use majik_core::db::{content_type_for_file, Db, ASSETS_PREFIX};
use majik_core::images::{gradient_png, Rng};
use majik_core::model::{
    Album, AlbumId, Asset, AssetId, Generation, GenerationId, GenerationInput, GenerationJob, JobId, JobStatus, MediaType, Status, ToolId,
};
use majik_core::{content_hash, now_ms, thumbnails, video};
use majik_providers::mock::image_renderer::{fit_longest_edge, parse_ratio};
use majik_providers::{
    catalog, voices, AspectRatio, AssetRole, AudioGenerationSettings, ImageGenerationSettings, ImageResolution, ProviderDescriptor, ProviderId,
    ProviderRegistry, VideoAspectRatio, VideoGenerationSettings, VideoResolution,
};
use majik_storage::{BlobStore, LocalBlobStore};

use crate::request::{GenerationType, Request};

/// What to put in the library. Everything is derived from [`SeedOptions::seed`], so the same
/// options produce the same library twice.
#[derive(Clone, Debug)]
pub struct SeedOptions {
    /// Library root (created if missing).
    pub root: PathBuf,
    pub images: usize,
    pub videos: usize,
    pub audio: usize,
    /// Assets imported into the library that no generation produced (the Assets feed lists them).
    pub imports: usize,
    pub albums: usize,
    /// How many distinct images to render and reuse across the rows. 0 renders every row's file
    /// separately: every tile different, but a PNG encode per row.
    pub pool: usize,
    /// Longest edge of the rendered images; picks the resolution the requests claim
    /// (512 = 0.5K, 1024 = 1K, 2048 = 2K, 3840 = 4K).
    pub long_edge: u32,
    /// Creation dates are spread backwards over this many days.
    pub days: u64,
    /// Render the thumbnails too, instead of leaving them to the app's background pass.
    pub thumbnails: bool,
    pub threads: usize,
    pub seed: u64,
    /// Delete an existing library at `root` first. Without it, seeding adds to what is there.
    pub reset: bool,
    /// Print progress to stdout (the CLI sets it; tests don't).
    pub progress: bool,
}

impl SeedOptions {
    /// Defaults at `root`: 2000 images, 200 clips, 100 tracks.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            images: 2000,
            videos: 200,
            audio: 100,
            imports: 100,
            albums: 8,
            pool: 64,
            long_edge: 1024,
            days: 365,
            thumbnails: false,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            seed: 1,
            reset: false,
            progress: false,
        }
    }

    fn total(&self) -> usize {
        self.images + self.videos + self.audio
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub generations: usize,
    pub assets: usize,
    pub albums: usize,
    pub thumbnails: usize,
    /// Bytes of media written (thumbnails and the database not counted).
    pub bytes: u64,
    pub elapsed_ms: u64,
}

impl SeedReport {
    /// One line for the CLI.
    pub fn describe(&self) -> String {
        format!(
            "{} generations, {} assets, {} albums, {} thumbnails, {:.2} GB in {:.1}s",
            self.generations,
            self.assets,
            self.albums,
            self.thumbnails,
            self.bytes as f64 / 1e9,
            Duration::from_millis(self.elapsed_ms).as_secs_f64()
        )
    }
}

/// Rows per transaction: big enough that commits disappear into the run, small enough that a
/// failure doesn't roll the whole thing back.
const BATCH: usize = 500;

/// Video is only ever rendered in these shapes; a row's request picks among them so the clip on
/// disk has the aspect ratio the request asked for.
const VIDEO_RATIOS: [VideoAspectRatio; 3] = [VideoAspectRatio::Landscape, VideoAspectRatio::Tall, VideoAspectRatio::Square];

/// Writes the library described by `options`.
pub fn seed_library(options: &SeedOptions) -> Result<SeedReport> {
    let started = Instant::now();
    prepare_root(options)?;
    let store = Arc::new(LocalBlobStore::new(&options.root));
    let mut db = Db::open(&options.root.join(".majik/library.db")).context("opening the library database")?;

    let pools = Arc::new(Pools::render(options)?);
    let plan = Arc::new(plan_rows(options, &pools));
    if options.progress {
        println!("writing {} generations on {} threads", plan.len(), options.threads.max(1));
    }

    let (rows, bytes) = write_rows(options, &plan, &pools, &store, &mut db)?;
    let mut report = SeedReport { generations: plan.len(), assets: rows.iter().filter(|row| row.asset.is_some()).count(), albums: options.albums, bytes, ..SeedReport::default() };

    let imported = write_imports(options, &store, &mut db)?;
    report.assets += imported.len();
    report.bytes += imported.iter().map(|(_, size)| size).sum::<u64>();

    let outputs: Vec<AssetId> = rows.iter().filter_map(|row| row.asset.clone()).collect();
    attach_inputs(options, &plan, &outputs, &imported, &mut db)?;
    fill_albums(options, &plan, &mut db)?;

    if options.thumbnails {
        report.thumbnails = write_thumbnails(options, &rows, &store, &mut db)?;
    }
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// A seed target has to be missing, empty, or a library already: seeding writes files into it and
/// `reset` deletes them again, and neither should ever happen to some other folder.
fn prepare_root(options: &SeedOptions) -> Result<()> {
    let root = &options.root;
    if root.exists() {
        let is_library = root.join(".majik").exists();
        let is_empty = root.read_dir().map(|mut entries| entries.next().is_none()).unwrap_or(false);
        if !is_library && !is_empty {
            bail!("{} is neither empty nor a majik library — refusing to seed into it", root.display());
        }
        if options.reset && is_library {
            if options.progress {
                println!("resetting {}", root.display());
            }
            let cache = root.join(".majik");
            std::fs::remove_dir_all(&cache).with_context(|| format!("removing {}", cache.display()))?;
            for entry in std::fs::read_dir(root)? {
                let path = entry?.path();
                let is_media = path.extension().and_then(|ext| ext.to_str()).and_then(MediaType::from_extension).is_some();
                if is_media && path.is_file() {
                    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
                }
            }
        }
    }
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    std::fs::create_dir_all(root.join(".majik"))?;
    Ok(())
}

// ----- the plan -------------------------------------------------------------------------------

/// One row to write: everything is decided up front, so the workers only render and write.
struct PlannedRow {
    id: GenerationId,
    request: Request,
    status: Status,
    favorite: bool,
    is_upscaled: bool,
    created_at_ms: u64,
    /// Which rendered file this row's output comes from (a pool index, or a seed of its own).
    content: u64,
    /// Tool rows and a share of the image rows carry an input asset, linked once the outputs exist.
    wants_input: bool,
    /// Rows that also carry a failed first attempt, so the attempt history isn't all one deep.
    retried: bool,
}

impl PlannedRow {
    fn media_type(&self) -> MediaType {
        self.request.generation_type.media_type()
    }
}

fn plan_rows(options: &SeedOptions, pools: &Pools) -> Vec<PlannedRow> {
    let mut rng = Rng::new(options.seed);
    let now = now_ms();
    let total = options.total().max(1);
    let step = options.days.saturating_mul(24 * 60 * 60 * 1000).max(total as u64) / total as u64;
    let mut rows = Vec::with_capacity(options.total());
    for index in 0..options.total() {
        let media = if index < options.images {
            MediaType::Image
        } else if index < options.images + options.videos {
            MediaType::Video
        } else {
            MediaType::Audio
        };
        // Newest first, spread backwards with jitter so the dates aren't a perfect ladder.
        let jitter = u64::from(rng.range(0, step.min(u64::from(u32::MAX)) as u32));
        let created_at_ms = now.saturating_sub(index as u64 * step + jitter);
        let request = plan_request(options, pools, media, &mut rng);
        let tool = request.generation_type.tool();
        rows.push(PlannedRow {
            id: GenerationId::new(),
            favorite: rng.chance(0.12),
            is_upscaled: tool == Some(ToolId::Upscale),
            wants_input: tool.is_some() || (media == MediaType::Image && rng.chance(0.1)),
            retried: rng.chance(0.05),
            status: plan_status(&mut rng),
            content: rng.next_u64(),
            created_at_ms,
            request,
        });
    }
    rows
}

/// The status mix a library ends up with: mostly completed, a few failures, a few files the user
/// moved away behind the app's back, and the odd row that never came back.
fn plan_status(rng: &mut Rng) -> Status {
    let roll = rng.unit();
    if roll < 0.03 {
        Status::Failed
    } else if roll < 0.045 {
        // Written, then removed: the library reports it as Missing when it opens.
        Status::Missing
    } else if roll < 0.05 {
        Status::Generating
    } else {
        Status::Completed
    }
}

fn plan_request(options: &SeedOptions, pools: &Pools, media: MediaType, rng: &mut Rng) -> Request {
    let providers = providers_for(media);
    let fallback = ProviderRegistry::shared().descriptor(&ProviderId::fal());
    let Some(descriptor) = rng.pick(&providers).copied().or(fallback) else {
        // Unreachable with the built-in registry; a request still has to come back.
        return Request::new(ProviderId::fal(), image_settings(pools, rng, options.long_edge), prompt(rng), Vec::new());
    };
    let prompt = prompt(rng);
    match media {
        MediaType::Image => {
            // A share of the image rows are tool runs (Upscale / Remove Background) instead.
            if !descriptor.supported_tool_models.is_empty() && rng.chance(0.06) {
                if let Some(model) = rng.pick(&descriptor.supported_tool_models) {
                    return Request::new(descriptor.id.clone(), GenerationType::for_tool_model(model), "", Vec::new());
                }
            }
            let mut settings = match image_settings(pools, rng, options.long_edge) {
                GenerationType::Image(settings) => settings,
                other => return Request::new(descriptor.id.clone(), other, prompt, Vec::new()),
            };
            if let Some(model) = rng.pick(&descriptor.supported_image_models) {
                settings.model = model.clone();
            }
            Request::new(descriptor.id.clone(), GenerationType::Image(settings), prompt, Vec::new())
        }
        MediaType::Video => {
            let model = rng.pick(&descriptor.supported_video_models).cloned().unwrap_or_else(|| catalog::video::ALL[0].clone());
            let capabilities = (descriptor.video_capabilities)(&model);
            let durations = capabilities.as_ref().map(|c| c.duration_range.presets_or_range()).unwrap_or_else(|| vec![5]);
            // Only the shapes the clip pool actually renders, so the file matches the request.
            let ratios: Vec<VideoAspectRatio> = match &capabilities {
                Some(c) => VIDEO_RATIOS.into_iter().filter(|r| c.aspect_ratios.contains(r)).collect(),
                None => VIDEO_RATIOS.to_vec(),
            };
            let aspect_ratio = rng.pick(&ratios).copied().unwrap_or(VideoAspectRatio::Landscape);
            let audio_enabled = capabilities.as_ref().is_some_and(|c| c.audio_always_on || (c.supports_audio && rng.chance(0.5)));
            let settings = VideoGenerationSettings {
                model,
                aspect_ratio: Some(aspect_ratio),
                resolution: Some(VideoResolution::Hd),
                duration: rng.pick(&durations).copied().unwrap_or(5),
                audio_enabled,
            };
            Request::new(descriptor.id.clone(), GenerationType::Video(settings), prompt, Vec::new())
        }
        MediaType::Audio => {
            let model = rng.pick(&descriptor.supported_audio_models).cloned().unwrap_or_else(|| catalog::audio::ALL[0].clone());
            let all = voices::elevenlabs::fal_voices();
            let speaker1 = rng.pick(all).cloned().unwrap_or_else(|| voices::elevenlabs::fal_default_voice().clone());
            let speaker2 = if rng.chance(0.25) { rng.pick(all).cloned() } else { None };
            Request::new(descriptor.id.clone(), GenerationType::Audio(AudioGenerationSettings { model, speaker1, speaker2 }), prompt, Vec::new())
        }
    }
}

/// Image settings whose aspect ratio is one the pool rendered (or any of them when every row gets
/// its own file).
fn image_settings(pools: &Pools, rng: &mut Rng, long_edge: u32) -> GenerationType {
    let ratios = pools.image_ratios();
    let aspect_ratio = rng.pick(&ratios).copied().unwrap_or(AspectRatio::Square);
    GenerationType::Image(ImageGenerationSettings { model: catalog::image::ALL[0].clone(), aspect_ratio, resolution: resolution_for(long_edge) })
}

/// The real providers that can make `media` (Mock is left out: a seeded row should name a provider
/// the user actually generates with).
fn providers_for(media: MediaType) -> Vec<&'static ProviderDescriptor> {
    ProviderRegistry::shared()
        .all()
        .into_iter()
        .filter(|descriptor| descriptor.id != ProviderId::mock())
        .filter(|descriptor| match media {
            MediaType::Image => !descriptor.supported_image_models.is_empty(),
            MediaType::Video => !descriptor.supported_video_models.is_empty(),
            MediaType::Audio => !descriptor.supported_audio_models.is_empty(),
        })
        .collect()
}

fn resolution_for(long_edge: u32) -> ImageResolution {
    match long_edge {
        0..=640 => ImageResolution::Sd,
        641..=1400 => ImageResolution::Hd,
        1401..=2800 => ImageResolution::Fhd,
        _ => ImageResolution::Uhd,
    }
}

fn image_size(long_edge: u32, aspect_ratio: AspectRatio) -> (u32, u32) {
    let (num, denom) = parse_ratio(aspect_ratio.raw(), (1, 1));
    fit_longest_edge(long_edge, num, denom)
}

fn video_size(long_edge: u32, aspect_ratio: VideoAspectRatio) -> (u32, u32) {
    let raw = aspect_ratio.raw();
    let (num, denom) = parse_ratio(if raw == "auto" { "16:9" } else { raw }, (16, 9));
    fit_longest_edge(long_edge, num, denom)
}

// ----- rendered content -----------------------------------------------------------------------

struct PoolImage {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

struct PoolClip {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    seconds: f64,
    aspect_ratio: VideoAspectRatio,
}

/// The files the rows draw from. Rendering one file per row is the most realistic but costs a PNG
/// encode per row; a pool keeps the grid varied for a fraction of the time. Clips and tracks are
/// always pooled, since an H.264 encode per row would dominate any run.
struct Pools {
    /// One bucket per [`AspectRatio::ALL`] entry; empty when every row renders its own image.
    images: Vec<Vec<PoolImage>>,
    clips: Vec<PoolClip>,
    tracks: Vec<(Vec<u8>, f64)>,
    long_edge: u32,
}

impl Pools {
    fn render(options: &SeedOptions) -> Result<Self> {
        let mut images: Vec<Vec<PoolImage>> = AspectRatio::ALL.iter().map(|_| Vec::new()).collect();
        if options.pool > 0 && options.images > 0 {
            let count = options.pool.min(options.images);
            if options.progress {
                println!("rendering {count} images at {} px", options.long_edge);
            }
            let plan: Vec<(usize, u64)> = (0..count).map(|index| (index % AspectRatio::ALL.len(), options.seed.wrapping_mul(0x9E37).wrapping_add(index as u64))).collect();
            let long_edge = options.long_edge;
            let rendered = in_parallel(options.threads, plan, move |(ratio, seed)| {
                let (width, height) = image_size(long_edge, AspectRatio::ALL[*ratio]);
                Ok((*ratio, PoolImage { bytes: gradient_png(width, height, *seed), width, height }))
            })?;
            for (ratio, image) in rendered {
                images[ratio].push(image);
            }
        }

        let clip_plan: Vec<(usize, u64)> = (0..options.videos.min(6)).map(|index| (index % VIDEO_RATIOS.len(), index as u64)).collect();
        if options.progress && !clip_plan.is_empty() {
            println!("encoding {} clips", clip_plan.len());
        }
        let seed = options.seed;
        let clips = in_parallel(options.threads, clip_plan, move |(ratio, index)| {
            let mut rng = Rng::new(seed ^ (index + 1));
            let aspect_ratio = VIDEO_RATIOS[*ratio];
            let (width, height) = video_size(1280, aspect_ratio);
            let seconds = rng.range(3, 6);
            let color = [rng.range(20, 235) as u8, rng.range(20, 235) as u8, rng.range(20, 235) as u8];
            let bytes = video::encode_clip(width, height, seconds * 24, 24, 24, color).map_err(|e| anyhow::anyhow!("encoding a seed clip: {e}"))?;
            Ok(PoolClip { bytes, width, height, seconds: f64::from(seconds), aspect_ratio })
        })?;

        let tracks = (0..options.audio.min(8))
            .map(|index| {
                let mut rng = Rng::new(options.seed ^ 0xA0D1 ^ index as u64);
                let seconds = f64::from(rng.range(4, 20));
                (tone_wav(seconds, 180. + rng.unit() * 400.), seconds)
            })
            .collect();

        Ok(Self { images, clips, tracks, long_edge: options.long_edge })
    }

    /// The aspect ratios a row may ask for: what the pool holds, or all of them when there is none.
    fn image_ratios(&self) -> Vec<AspectRatio> {
        let available: Vec<AspectRatio> =
            AspectRatio::ALL.iter().enumerate().filter(|(index, _)| self.images.get(*index).is_some_and(|bucket| !bucket.is_empty())).map(|(_, ratio)| *ratio).collect();
        if available.is_empty() {
            AspectRatio::ALL.to_vec()
        } else {
            available
        }
    }

    /// The bytes for an image row: a pool entry of the right shape, else one rendered for this row.
    fn image(&self, aspect_ratio: AspectRatio, content: u64) -> (Vec<u8>, u32, u32) {
        let bucket = AspectRatio::ALL.iter().position(|ratio| *ratio == aspect_ratio).and_then(|index| self.images.get(index)).filter(|bucket| !bucket.is_empty());
        match bucket {
            Some(bucket) => {
                let image = &bucket[(content % bucket.len() as u64) as usize];
                (image.bytes.clone(), image.width, image.height)
            }
            None => {
                let (width, height) = image_size(self.long_edge, aspect_ratio);
                (gradient_png(width, height, content), width, height)
            }
        }
    }

    fn clip(&self, aspect_ratio: VideoAspectRatio, content: u64) -> Option<&PoolClip> {
        let matching: Vec<&PoolClip> = self.clips.iter().filter(|clip| clip.aspect_ratio == aspect_ratio).collect();
        let pick = if matching.is_empty() { self.clips.iter().collect::<Vec<_>>() } else { matching };
        pick.get((content % pick.len().max(1) as u64) as usize).copied()
    }

    fn track(&self, content: u64) -> Option<&(Vec<u8>, f64)> {
        self.tracks.get((content % self.tracks.len().max(1) as u64) as usize)
    }
}

/// A 16-bit mono WAV of a fading tone: small, real audio to probe, draw and play back.
pub fn tone_wav(seconds: f64, hz: f32) -> Vec<u8> {
    const RATE: u32 = 22_050;
    let seconds = seconds.max(0.1);
    let frames = (seconds * f64::from(RATE)) as u32;
    let data_len = frames * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..frames {
        let t = f64::from(frame) / f64::from(RATE);
        // A slow tremolo and a fade-out, so the waveform has a shape rather than a flat band.
        let envelope = (1. - t / seconds).clamp(0., 1.) * (0.6 + 0.4 * (t * 2.).sin());
        let sample = (t * f64::from(hz) * std::f64::consts::TAU).sin() * envelope * 12_000.;
        out.extend_from_slice(&(sample as i16).to_le_bytes());
    }
    out
}

/// Runs `f` over `items` on `threads` worker threads, keeping the input order.
fn in_parallel<In, Out>(threads: usize, items: Vec<In>, f: impl Fn(&In) -> Result<Out> + Sync) -> Result<Vec<Out>>
where
    In: Send + Sync,
    Out: Send,
{
    let cursor = AtomicUsize::new(0);
    let collected: Mutex<Vec<(usize, Out)>> = Mutex::new(Vec::with_capacity(items.len()));
    let (items, f, cursor, collected) = (&items, &f, &cursor, &collected);
    std::thread::scope(|scope| -> Result<()> {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(move || -> Result<()> {
                    loop {
                        let index = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else { return Ok(()) };
                        let value = f(item)?;
                        collected.lock().map_err(|_| anyhow::anyhow!("a rendering thread panicked"))?.push((index, value));
                    }
                })
            })
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(result) => result?,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        Ok(())
    })?;
    let mut collected = collected.lock().map_err(|_| anyhow::anyhow!("a rendering thread panicked"))?.drain(..).collect::<Vec<_>>();
    collected.sort_by_key(|(index, _)| *index);
    Ok(collected.into_iter().map(|(_, value)| value).collect())
}

// ----- writing --------------------------------------------------------------------------------

/// What a worker produced for one planned row.
struct WrittenRow {
    generation: Generation,
    jobs: Vec<GenerationJob>,
    /// The output asset, for the input links and the thumbnail pass.
    asset: Option<AssetId>,
    /// The asset row and its blob key; inserted before the generation that points at it.
    asset_row: Option<(Asset, String)>,
    /// Path and kind of the file on disk, for the thumbnail pass (`None` once it is a missing row).
    file: Option<(PathBuf, MediaType)>,
    bytes: u64,
}

fn write_rows(options: &SeedOptions, plan: &Arc<Vec<PlannedRow>>, pools: &Arc<Pools>, store: &Arc<LocalBlobStore>, db: &mut Db) -> Result<(Vec<WrittenRow>, u64)> {
    let cursor = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::sync_channel::<Result<Vec<WrittenRow>>>(options.threads.max(1) * 2);
    let mut written = Vec::with_capacity(plan.len());
    let mut bytes = 0u64;

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..options.threads.max(1) {
            let (cursor, plan, pools, store, sender) = (cursor.clone(), plan.clone(), pools.clone(), store.clone(), sender.clone());
            scope.spawn(move || loop {
                let start = cursor.fetch_add(BATCH, Ordering::Relaxed);
                if start >= plan.len() {
                    return;
                }
                let end = (start + BATCH).min(plan.len());
                let batch = plan[start..end].iter().map(|row| write_row(row, &pools, store.as_ref())).collect::<Result<Vec<_>>>();
                if sender.send(batch).is_err() {
                    return;
                }
            });
        }
        drop(sender);

        for batch in receiver {
            let batch = batch?;
            db.transaction(|db| {
                for row in &batch {
                    insert_row(db, row)?;
                }
                Ok(())
            })?;
            bytes += batch.iter().map(|row| row.bytes).sum::<u64>();
            written.extend(batch);
            if options.progress && written.len() % (BATCH * 4) < BATCH {
                println!("  {} / {} generations", written.len(), plan.len());
            }
        }
        Ok(())
    })?;
    Ok((written, bytes))
}

/// Renders one row's file and builds its rows: the seeding equivalent of `add_generating`
/// followed by `complete_generation`.
fn write_row(planned: &PlannedRow, pools: &Pools, store: &dyn BlobStore) -> Result<WrittenRow> {
    let media_type = planned.media_type();
    let request_json = serde_json::to_string(&planned.request).ok();
    let mut written = WrittenRow {
        generation: Generation {
            id: planned.id.clone(),
            path: None,
            media_type,
            status: planned.status,
            created_at_ms: planned.created_at_ms,
            width: None,
            height: None,
            duration_secs: None,
            file_size: None,
            is_favorite: planned.favorite,
            is_upscaled: planned.is_upscaled,
            thumbnail: None,
            output_asset_id: None,
            request_json: request_json.clone(),
            model_name: Some(planned.request.generation_type.model_name().to_string()),
            provider: Some(planned.request.provider.as_str().to_string()),
            error: None,
            error_kind: None,
            tool: planned.request.generation_type.tool(),
            job_id: None,
            poll_url: None,
            started_at_ms: None,
            active_job_id: None,
        },
        jobs: Vec::new(),
        asset: None,
        asset_row: None,
        file: None,
        bytes: 0,
    };

    if matches!(planned.status, Status::Completed | Status::Missing) {
        let file_name = format!("{}.{}", planned.id, file_extension(media_type));
        let Rendered { bytes, width, height, duration_secs } = render(planned, pools)?;
        store.put(&file_name, &bytes).with_context(|| format!("writing {file_name}"))?;
        let path = store.local_path(&file_name)?;
        let asset = Asset {
            id: AssetId::new(),
            content_hash: Some(content_hash(&bytes)),
            kind: media_type,
            content_type: content_type_for_file(&file_name, media_type).to_string(),
            path: path.clone(),
            width,
            height,
            file_size: Some(bytes.len() as u64),
            duration_secs,
            created_at_ms: planned.created_at_ms,
            thumbnail: None,
            missing: false,
        };
        written.bytes = bytes.len() as u64;
        written.generation.output_asset_id = Some(asset.id.clone());
        written.generation.path = Some(path.clone());
        written.generation.width = width;
        written.generation.height = height;
        written.generation.duration_secs = duration_secs;
        written.generation.file_size = asset.file_size;
        written.asset = Some(asset.id.clone());
        written.asset_row = Some((asset, file_name.clone()));
        if planned.status == Status::Missing {
            // The row and its asset stay; the file is what the user moved away.
            store.delete(&file_name)?;
            written.bytes = 0;
        } else {
            written.file = Some((path, media_type));
        }
    }

    written.jobs = plan_jobs(planned, written.asset.clone(), request_json);
    written.generation.active_job_id = written.jobs.last().map(|job| job.id.clone());
    Ok(written)
}

/// Seeded audio is a WAV (real samples, no encoder needed); images and video match what a provider
/// returns.
fn file_extension(media_type: MediaType) -> &'static str {
    match media_type {
        MediaType::Audio => "wav",
        other => other.file_extension(),
    }
}

/// One row's output file.
struct Rendered {
    bytes: Vec<u8>,
    width: Option<u32>,
    height: Option<u32>,
    duration_secs: Option<f64>,
}

fn render(planned: &PlannedRow, pools: &Pools) -> Result<Rendered> {
    match &planned.request.generation_type {
        // Tools return an image of their input's shape; square is as good a stand-in as any.
        GenerationType::Image(_) | GenerationType::Upscale(_) | GenerationType::RemoveBackground(_) => {
            let aspect_ratio = match &planned.request.generation_type {
                GenerationType::Image(settings) => settings.aspect_ratio,
                _ => AspectRatio::Square,
            };
            let (bytes, width, height) = pools.image(aspect_ratio, planned.content);
            Ok(Rendered { bytes, width: Some(width), height: Some(height), duration_secs: None })
        }
        GenerationType::Video(settings) => {
            let Some(clip) = pools.clip(settings.aspect_ratio.unwrap_or(VideoAspectRatio::Landscape), planned.content) else {
                bail!("no clips were rendered");
            };
            Ok(Rendered { bytes: clip.bytes.clone(), width: Some(clip.width), height: Some(clip.height), duration_secs: Some(clip.seconds) })
        }
        GenerationType::Audio(_) => {
            let Some((bytes, seconds)) = pools.track(planned.content) else {
                bail!("no audio was rendered");
            };
            Ok(Rendered { bytes: bytes.clone(), width: None, height: None, duration_secs: Some(*seconds) })
        }
    }
}

/// The attempts behind a row: the one it mirrors, preceded by a failed try on the retried rows.
fn plan_jobs(planned: &PlannedRow, output: Option<AssetId>, request_json: Option<String>) -> Vec<GenerationJob> {
    let mut jobs = Vec::new();
    let created = planned.created_at_ms.saturating_sub(45_000);
    if planned.retried {
        jobs.push(GenerationJob {
            id: JobId::new(),
            generation_id: planned.id.clone(),
            attempt: 1,
            status: JobStatus::Failed,
            external_id: Some(format!("seed-{}-1", planned.id.as_str())),
            poll_url: None,
            output_asset_id: None,
            error: Some("The provider was overloaded. Try again.".into()),
            error_kind: Some("rateLimited".into()),
            provider_request_json: request_json.clone(),
            provider_create_response_json: Some(r#"{"status":"IN_QUEUE"}"#.into()),
            provider_final_response_json: Some(r#"{"detail":"rate limit exceeded"}"#.into()),
            created_at_ms: created.saturating_sub(60_000),
            started_at_ms: Some(created.saturating_sub(59_000)),
            finished_at_ms: Some(created.saturating_sub(30_000)),
        });
    }
    let attempt = jobs.len() as u32 + 1;
    let (status, error, error_kind) = match planned.status {
        Status::Completed | Status::Missing => (JobStatus::Completed, None, None),
        Status::Failed => (JobStatus::Failed, Some("The model refused this prompt.".to_string()), Some("contentPolicy".to_string())),
        Status::Generating => (JobStatus::Running, None, None),
    };
    let running = status == JobStatus::Running;
    jobs.push(GenerationJob {
        id: JobId::new(),
        generation_id: planned.id.clone(),
        attempt,
        status,
        external_id: Some(format!("seed-{}-{attempt}", planned.id.as_str())),
        poll_url: running.then(|| format!("https://queue.example/requests/seed-{}", planned.id.as_str())),
        output_asset_id: output,
        error,
        error_kind,
        provider_request_json: request_json,
        provider_create_response_json: Some(r#"{"status":"IN_QUEUE","queue_position":0}"#.into()),
        provider_final_response_json: (!running).then(|| r#"{"status":"COMPLETED"}"#.to_string()),
        created_at_ms: created,
        started_at_ms: Some(created + 500),
        finished_at_ms: (!running).then_some(planned.created_at_ms),
    });
    jobs
}

fn insert_row(db: &Db, row: &WrittenRow) -> Result<()> {
    if let Some((asset, key)) = &row.asset_row {
        db.insert_asset(asset, key)?;
    }
    // The row first without its attempt (the attempts reference it), then the pointer back.
    let mut generation = row.generation.clone();
    generation.active_job_id = None;
    db.upsert_generation(&generation)?;
    for job in &row.jobs {
        db.insert_job(job)?;
    }
    if let Some(job) = row.jobs.last() {
        db.set_active_job(&row.generation.id, &job.id)?;
    }
    Ok(())
}

/// Assets the user brought in rather than generated: the Assets feed lists them and the composer's
/// role cards hold them.
fn write_imports(options: &SeedOptions, store: &Arc<LocalBlobStore>, db: &mut Db) -> Result<Vec<(AssetId, u64)>> {
    if options.imports == 0 {
        return Ok(Vec::new());
    }
    if options.progress {
        println!("importing {} assets", options.imports);
    }
    let seeds: Vec<u64> = (0..options.imports as u64).map(|index| options.seed ^ 0x1_0000 ^ index).collect();
    let long_edge = options.long_edge;
    let store_ref: &LocalBlobStore = store.as_ref();
    let assets = in_parallel(options.threads, seeds, move |seed| {
        let bytes = gradient_png(long_edge, long_edge, *seed);
        let hash = content_hash(&bytes);
        let key = format!("{ASSETS_PREFIX}/{hash}.png");
        store_ref.put(&key, &bytes)?;
        Ok((
            Asset {
                id: AssetId::new(),
                content_hash: Some(hash),
                kind: MediaType::Image,
                content_type: "image/png".into(),
                path: store_ref.local_path(&key)?,
                width: Some(long_edge),
                height: Some(long_edge),
                file_size: Some(bytes.len() as u64),
                duration_secs: None,
                created_at_ms: now_ms(),
                thumbnail: None,
                missing: false,
            },
            key,
        ))
    })?;
    let mut written = Vec::with_capacity(assets.len());
    db.transaction(|db| {
        for (asset, key) in &assets {
            db.insert_asset(asset, key)?;
            written.push((asset.id.clone(), asset.file_size.unwrap_or_default()));
        }
        Ok(())
    })?;
    Ok(written)
}

/// Links the rows that asked for an input to an asset that already exists: a tool row to the image
/// it ran on, an image row to its reference image.
fn attach_inputs(options: &SeedOptions, plan: &[PlannedRow], outputs: &[AssetId], imports: &[(AssetId, u64)], db: &mut Db) -> Result<()> {
    let mut candidates: Vec<AssetId> = imports.iter().map(|(id, _)| id.clone()).collect();
    candidates.extend(outputs.iter().cloned());
    if candidates.is_empty() {
        return Ok(());
    }
    let mut rng = Rng::new(options.seed ^ 0xBEEF);
    let roles = [AssetRole::ReferenceImage, AssetRole::ControlImage];
    let links: Vec<GenerationInput> = plan
        .iter()
        .filter(|row| row.wants_input)
        .filter_map(|row| {
            let asset_id = candidates.get(rng.below(candidates.len()))?.clone();
            let role = if row.request.generation_type.tool().is_some() { AssetRole::ReferenceImage } else { *rng.pick(&roles)? };
            Some(GenerationInput { generation_id: row.id.clone(), asset_id, role: role.raw().to_string(), position: 0 })
        })
        .collect();
    for chunk in links.chunks(BATCH) {
        db.transaction(|db| {
            for link in chunk {
                db.insert_input(link)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn fill_albums(options: &SeedOptions, plan: &[PlannedRow], db: &mut Db) -> Result<()> {
    if options.albums == 0 || plan.is_empty() {
        return Ok(());
    }
    let mut rng = Rng::new(options.seed ^ 0xA1B0);
    let now = now_ms();
    db.transaction(|db| {
        for index in 0..options.albums {
            let name = format!("{} {}", rng.pick(&ALBUM_NAMES).copied().unwrap_or("Album"), index + 1);
            let album = Album { id: AlbumId::new(), name, created_at_ms: now.saturating_sub(index as u64 * 86_400_000), items: Vec::new() };
            db.insert_album(&album)?;
            // Every fourth album is a feed of its own to scroll; the rest are small.
            let size = if index % 4 == 0 { plan.len() / 4 } else { plan.len() / 40 };
            let members: Vec<GenerationId> = (0..size).filter_map(|_| plan.get(rng.below(plan.len())).map(|row| row.id.clone())).collect();
            db.add_to_album(&album.id, &members, now)?;
        }
        Ok(())
    })
}

fn write_thumbnails(options: &SeedOptions, rows: &[WrittenRow], store: &Arc<LocalBlobStore>, db: &mut Db) -> Result<usize> {
    let work: Vec<(AssetId, PathBuf, MediaType)> = rows
        .iter()
        .filter_map(|row| {
            let (path, kind) = row.file.clone()?;
            let asset = row.asset.clone()?;
            (kind != MediaType::Audio).then_some((asset, path, kind))
        })
        .collect();
    if work.is_empty() {
        return Ok(0);
    }
    if options.progress {
        println!("rendering {} thumbnails", work.len());
    }
    let cursor = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::sync_channel::<Vec<(AssetId, PathBuf)>>(options.threads.max(1) * 2);
    let work = Arc::new(work);
    let mut done = 0;

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..options.threads.max(1) {
            let (cursor, work, store, sender) = (cursor.clone(), work.clone(), store.clone(), sender.clone());
            scope.spawn(move || loop {
                let start = cursor.fetch_add(BATCH, Ordering::Relaxed);
                if start >= work.len() {
                    return;
                }
                let end = (start + BATCH).min(work.len());
                let batch: Vec<(AssetId, PathBuf)> = work[start..end]
                    .iter()
                    .filter_map(|(id, path, kind)| match thumbnails::ensure_thumbnail_for(path, *kind, store.as_ref()) {
                        Ok(thumb) => Some((id.clone(), thumb)),
                        Err(e) => {
                            tracing::warn!(target: "majik", "seed: thumbnailing {}: {e:#}", path.display());
                            None
                        }
                    })
                    .collect();
                if sender.send(batch).is_err() {
                    return;
                }
            });
        }
        drop(sender);
        for batch in receiver {
            db.transaction(|db| {
                for (id, thumb) in &batch {
                    db.set_asset_thumbnail(id, Some(thumb))?;
                }
                Ok(())
            })?;
            done += batch.len();
        }
        Ok(())
    })?;
    Ok(done)
}

const ALBUM_NAMES: [&str; 8] = ["Moodboard", "Client work", "Keepers", "Storyboard", "References", "Product shots", "Loops", "Scratch"];

// ----- prompts --------------------------------------------------------------------------------

const SUBJECTS: [&str; 12] = [
    "a lighthouse on a basalt shore",
    "an astronaut resting on a dune",
    "a rain-slick Tokyo alley",
    "a glass conservatory full of ferns",
    "a fox curled in tall grass",
    "an abandoned funicular station",
    "a market stall at dawn",
    "a paper boat on black water",
    "a cathedral of scaffolding",
    "twin moons over a salt flat",
    "a mechanic's hands and a warm engine",
    "a diner counter at 3am",
];
const STYLES: [&str; 8] = [
    "shot on 35mm film",
    "in the style of a woodblock print",
    "hyperreal product photography",
    "loose gouache illustration",
    "isometric diorama",
    "long-exposure night photography",
    "cel-shaded animation still",
    "tilt-shift miniature",
];
const DETAILS: [&str; 8] = [
    "volumetric fog",
    "warm rim lighting",
    "muted teal and amber palette",
    "shallow depth of field",
    "hard noon shadows",
    "practical neon signage",
    "fine grain, slight halation",
    "wide establishing framing",
];

fn prompt(rng: &mut Rng) -> String {
    let mut parts = vec![rng.pick(&SUBJECTS).copied().unwrap_or("a landscape").to_string()];
    if rng.chance(0.8) {
        parts.push(rng.pick(&STYLES).copied().unwrap_or("photographic").to_string());
    }
    let extra = rng.range(0, 3) + if rng.chance(0.05) { 12 } else { 0 };
    for _ in 0..extra {
        parts.push(rng.pick(&DETAILS).copied().unwrap_or("soft light").to_string());
    }
    parts.join(", ")
}

/// How to launch the app against a seeded library, for the CLI to print.
pub fn launch_hint(root: &Path) -> String {
    format!("MAJIK_LIBRARY={} cargo run --release -p majik-app", root.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_core::{FeedFilter, Library, MediaFilter};
    use tempfile::TempDir;

    /// Small but complete: every media type, every status, thumbnails on. 64 px files keep it fast.
    fn options(dir: &TempDir) -> SeedOptions {
        SeedOptions {
            images: 60,
            videos: 2,
            audio: 2,
            imports: 3,
            albums: 2,
            pool: 8,
            long_edge: 64,
            threads: 4,
            ..SeedOptions::at(dir.path())
        }
    }

    fn seeded(options: &SeedOptions) -> (SeedReport, Library) {
        let report = seed_library(options).unwrap();
        let library = Library::open(&options.root).unwrap();
        (report, library)
    }

    #[test]
    fn seeds_a_library_the_feed_can_open() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(&dir);
        let (report, library) = seeded(&options);

        assert_eq!(report.generations, options.total());
        assert_eq!(library.generations().len(), options.total());
        assert_eq!(library.feed(&FeedFilter::Library, MediaFilter::All).len(), options.total());
        for media in [MediaFilter::Image, MediaFilter::Video, MediaFilter::Audio] {
            assert!(!library.feed(&FeedFilter::Library, media).is_empty(), "{media:?} feed is empty");
        }
        // Newest first, the order the feed relies on.
        let dates: Vec<u64> = library.generations().iter().map(|row| row.created_at_ms).collect();
        assert!(dates.windows(2).all(|pair| pair[0] >= pair[1]), "rows are not newest first");
    }

    #[test]
    fn covers_every_status_a_row_can_be_in() {
        let dir = tempfile::tempdir().unwrap();
        // 400 rows: enough that the 3% / 1.5% / 0.5% tail of the status mix shows up.
        let (_, library) = seeded(&SeedOptions { images: 400, videos: 0, audio: 0, ..options(&dir) });
        let mut seen: Vec<Status> = library.generations().iter().map(|row| row.status).collect();
        seen.sort_by_key(|status| format!("{status:?}"));
        seen.dedup();
        for status in [Status::Completed, Status::Failed, Status::Missing, Status::Generating] {
            assert!(seen.contains(&status), "no {status:?} row among {seen:?}");
        }
        let failed = library.generations().iter().find(|row| row.status == Status::Failed).unwrap();
        assert!(failed.error.is_some(), "a failed row shows why");
        let generating = library.generations().iter().find(|row| row.status == Status::Generating).unwrap();
        assert!(generating.job_id.is_some(), "an in-flight row keeps its provider handle");
    }

    #[test]
    fn a_missing_row_keeps_its_asset_and_loses_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let (_, library) = seeded(&SeedOptions { images: 400, videos: 0, audio: 0, ..options(&dir) });
        let missing = library.generations().iter().find(|row| row.status == Status::Missing).expect("a missing row");
        let asset = library.asset(missing.output_asset_id.as_ref().unwrap()).unwrap();
        assert!(asset.missing, "the asset is marked missing");
        assert!(!asset.path.exists(), "{} is still on disk", asset.path.display());
    }

    #[test]
    fn every_row_stores_the_request_that_recreate_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let (_, library) = seeded(&options(&dir));
        for row in library.generations() {
            let json = row.request_json.as_ref().unwrap_or_else(|| panic!("{} has no request", row.id));
            let request: Request = serde_json::from_str(json).unwrap();
            assert_eq!(request.generation_type.media_type(), row.media_type);
            assert_eq!(request.generation_type.tool(), row.tool);
            assert_eq!(Some(request.provider.as_str()), row.provider.as_deref());
            assert!(row.can_recreate(), "{} cannot be recreated", row.id);
        }
        assert!(library.generations().iter().any(|row| row.tool == Some(ToolId::Upscale) || row.tool == Some(ToolId::RemoveBackground)), "no tool rows");
    }

    #[test]
    fn the_file_on_disk_matches_what_the_row_claims() {
        let dir = tempfile::tempdir().unwrap();
        let (_, library) = seeded(&options(&dir));
        for row in library.generations().iter().filter(|row| row.status == Status::Completed) {
            let path = row.path.as_ref().unwrap_or_else(|| panic!("{} has no file", row.id));
            assert!(path.exists(), "{} is not on disk", path.display());
            assert_eq!(MediaType::from_extension(path.extension().unwrap().to_str().unwrap()), Some(row.media_type));
            match row.media_type {
                MediaType::Image => {
                    let (width, height) = majik_core::thumbnails::image_dimensions(path).unwrap();
                    assert_eq!((Some(width), Some(height)), (row.width, row.height), "{} lies about its size", row.id);
                }
                MediaType::Video => assert!(video::probe(path).unwrap().duration_secs.is_some()),
                MediaType::Audio => assert!(row.duration_secs.is_some_and(|d| d > 0.)),
            }
        }
    }

    #[test]
    fn favorites_albums_and_assets_have_their_own_feeds() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(&dir);
        let (report, library) = seeded(&options);

        assert!(!library.feed(&FeedFilter::Favorites, MediaFilter::All).is_empty(), "nothing is a favorite");
        assert_eq!(library.albums().len(), options.albums);
        for album in library.albums() {
            assert!(!album.items.is_empty(), "album {} is empty", album.name);
            assert!(!library.feed(&FeedFilter::Album(album.id.clone()), MediaFilter::All).is_empty());
        }
        // Outputs plus the imports; a failed row has no asset, so this is a lower bound.
        assert_eq!(library.entries(&FeedFilter::Assets, MediaFilter::All).len(), report.assets);
        assert!(library.assets().len() >= options.imports);
    }

    #[test]
    fn imports_belong_to_no_generation_and_inputs_are_referenced() {
        let dir = tempfile::tempdir().unwrap();
        let (_, library) = seeded(&options(&dir));
        let imported: Vec<_> = library.assets().iter().filter(|asset| library.generation_producing(&asset.id).is_none()).collect();
        assert!(!imported.is_empty(), "no imported assets");
        assert!(imported.iter().all(|asset| asset.path.exists()));

        let with_inputs: Vec<_> = library.generations().iter().filter(|row| !library.inputs(&row.id).is_empty()).collect();
        assert!(!with_inputs.is_empty(), "no row has an input asset");
        for row in with_inputs {
            for (input, asset) in library.inputs(&row.id) {
                assert!(!input.role.is_empty());
                assert!(library.is_referenced(&asset.id), "{} is used but not referenced", asset.id);
            }
        }
    }

    #[test]
    fn attempts_are_written_including_a_retried_history() {
        let dir = tempfile::tempdir().unwrap();
        let (_, library) = seeded(&SeedOptions { images: 120, videos: 0, audio: 0, ..options(&dir) });
        for row in library.generations() {
            let jobs = library.jobs(&row.id);
            assert!(!jobs.is_empty(), "{} has no attempt", row.id);
            let active = library.active_job(&row.id).expect("an active attempt");
            assert_eq!(Some(&active.id), jobs.last().map(|job| &job.id), "the row mirrors its last attempt");
            assert!(active.provider_request_json.is_some(), "the attempt records what was sent");
            if row.status == Status::Completed {
                assert_eq!(active.status, JobStatus::Completed);
                assert_eq!(active.output_asset_id, row.output_asset_id);
            }
        }
        assert!(library.generations().iter().any(|row| library.jobs(&row.id).len() > 1), "no row was retried");
    }

    #[test]
    fn thumbnails_are_rendered_when_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let (report, library) = seeded(&SeedOptions { thumbnails: true, ..options(&dir) });
        assert!(report.thumbnails > 0);
        let visual: Vec<_> = library.generations().iter().filter(|row| row.status == Status::Completed && row.media_type != MediaType::Audio).collect();
        assert!(visual.iter().all(|row| row.thumbnail.as_ref().is_some_and(|thumb| thumb.exists())), "a thumbnail is missing after reopening");

        let bare = tempfile::tempdir().unwrap();
        let (report, library) = seeded(&SeedOptions { thumbnails: false, ..options(&bare) });
        assert_eq!(report.thumbnails, 0);
        assert!(library.generations().iter().all(|row| row.thumbnail.is_none()), "thumbnails were rendered anyway");
    }

    #[test]
    fn the_same_options_seed_the_same_library() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let (_, one) = seeded(&options(&first));
        let (_, two) = seeded(&options(&second));

        let shape = |library: &Library| -> Vec<(String, MediaType, Status, Option<u64>)> {
            library.generations().iter().map(|row| (row.prompt().unwrap_or_default(), row.media_type, row.status, row.file_size)).collect()
        };
        assert_eq!(shape(&one), shape(&two));

        let third = tempfile::tempdir().unwrap();
        let (_, other) = seeded(&SeedOptions { seed: 99, ..options(&third) });
        assert_ne!(shape(&one), shape(&other), "a different seed produces a different library");
    }

    #[test]
    fn seeding_again_adds_and_reset_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let options = SeedOptions { images: 10, videos: 0, audio: 0, imports: 1, albums: 1, ..options(&dir) };
        seed_library(&options).unwrap();
        seed_library(&options).unwrap();
        assert_eq!(Library::open(dir.path()).unwrap().generations().len(), 20);

        seed_library(&SeedOptions { reset: true, ..options.clone() }).unwrap();
        let library = Library::open(dir.path()).unwrap();
        assert_eq!(library.generations().len(), 10);
        assert_eq!(library.albums().len(), 1);
        // The previous run's files went with it.
        let files = std::fs::read_dir(dir.path()).unwrap().filter(|entry| entry.as_ref().is_ok_and(|e| e.path().is_file())).count();
        assert_eq!(files, library.generations().iter().filter(|row| row.status == Status::Completed).count());
    }

    #[test]
    fn refuses_a_folder_that_is_not_a_library() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "mine").unwrap();
        let error = seed_library(&options(&dir)).unwrap_err().to_string();
        assert!(error.contains("refusing to seed"), "{error}");
        assert!(dir.path().join("notes.txt").exists(), "it wrote anyway");
    }

    #[test]
    fn tone_wav_is_a_playable_pcm_file() {
        let bytes = tone_wav(0.5, 440.);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // 44-byte header plus 0.5s of 16-bit mono at 22.05 kHz.
        assert_eq!(bytes.len(), 44 + 11_025 * 2);
        assert!(bytes[44..].iter().any(|sample| *sample != 0), "the track is silent");
    }
}
