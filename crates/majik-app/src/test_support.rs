//! Shared harness for the headless GPUI view tests (Zed-style `TestAppContext`).
//!
//! Each test builds a real (offscreen) window over a temporary library seeded with generated
//! solid-colour PNGs, then drives the actual view logic and asserts on the view and the library model.
#![cfg(test)]

use anyhow::anyhow;
use gpui::{App, AppContext as _, Entity, Task, TestAppContext};
use majik_core::images::solid_png;
use majik_core::model::{AssetId, GenerationId, MediaType, Status, ToolId};
use majik_core::Library;
use majik_generation::engine::JobRunner;
use majik_generation::{improve_channel, GenerationType, ImproveReceiver, ImproveSender, Job, Request, TextRequest};
use majik_providers::{catalog, AspectRatio, ImageGenerationSettings, ImageResolution, ProviderId};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

use crate::config::Config;
use crate::credentials::{ApiKeys, KeyMap, SecretBackend};
use crate::state::{AppState, LibraryModel};
use crate::windows::Windows;

pub struct TestEnv {
    /// Kept alive so the temp library isn't removed mid-test.
    #[allow(dead_code)]
    pub dir: TempDir,
    pub library: Entity<LibraryModel>,
}

/// `n` completed generated images (the library only lists what the app generated; files dropped
/// into the folder are not items). Colours are deterministic so thumbnails are stable across runs.
pub fn seed_images(library: &mut Library, n: usize) {
    for i in 0..n {
        let rgb = [(i * 40 % 256) as u8, (i * 90 % 256) as u8, (i * 20 % 256) as u8];
        // Vary dimensions a little so aspect-ratio code paths differ.
        let (w, h) = if i % 3 == 0 { (64, 64) } else if i % 3 == 1 { (96, 48) } else { (48, 96) };
        let id = library.add_generating(MediaType::Image, None, None, None, None);
        library.complete_generation(&id, &solid_png(w, h, rgb), false).unwrap();
    }
}

/// Sets up globals (Config with the Mock provider, an in-memory key, an open library) and returns
/// the environment. Call inside `cx.update(...)`.
pub fn setup(cx: &mut App, images: usize, provider: &str) -> TestEnv {
    setup_with_keys(cx, images, provider, ApiKeys::in_memory([("Mock", "k"), ("fal.ai", "k"), ("Replicate", "k"), ("OpenRouter", "k")]))
}

/// [`setup`] over a specific [`JobRunner`], for tests that need to answer what the view submitted
/// (a prompt rewrite, say) rather than have it dropped.
pub fn setup_with_runner(cx: &mut App, images: usize, provider: &str, runner: Box<dyn JobRunner>) -> TestEnv {
    setup_inner(cx, images, provider, ApiKeys::in_memory([("Mock", "k"), ("fal.ai", "k"), ("Replicate", "k"), ("OpenRouter", "k")]), Some(runner))
}

/// [`setup`] with a specific key store, e.g. one over a [`TestBackend`] that fails.
pub fn setup_with_keys(cx: &mut App, images: usize, provider: &str, keys: ApiKeys) -> TestEnv {
    setup_inner(cx, images, provider, keys, None)
}

fn setup_inner(cx: &mut App, images: usize, provider: &str, keys: ApiKeys, runner: Option<Box<dyn JobRunner>>) -> TestEnv {
    gpui_component::init(cx);
    crate::ui::install_theme(cx);
    // Installs the keymap so `simulate_keystrokes` reaches the views' action handlers.
    crate::actions::init(cx);
    let dir = tempfile::tempdir().unwrap();

    let keys = Arc::new(keys);
    let keys_for_lib = keys.clone();
    let root = dir.path().to_path_buf();
    let library = cx.new(|cx| {
        let mut model = match runner {
            Some(runner) => LibraryModel::open_with_runner(root, runner, cx).unwrap(),
            None => LibraryModel::open_inert(root, keys_for_lib, cx).unwrap(),
        };
        seed_images(&mut model.lib, images);
        model
    });

    let config = Config { provider: provider.to_string(), onboarding_completed: false, ..Default::default() };
    cx.set_global(config);
    cx.set_global(AppState { library: library.clone(), keys });
    cx.set_global(Windows::default());

    TestEnv { dir, library }
}

/// A runner that only remembers what it was asked to do, for asserting on the jobs a relaunch
/// submits without any worker thread.
pub struct RecordingRunner {
    jobs: Arc<Mutex<Vec<Job>>>,
    rewrites: Rewrites,
}

/// The prompt rewrites a [`RecordingRunner`] was asked for, each with the sender the view is
/// waiting on, so a test decides when (and whether) one answers.
pub type Rewrites = Arc<Mutex<Vec<(TextRequest, ImproveSender)>>>;

impl RecordingRunner {
    pub fn new() -> (Self, Arc<Mutex<Vec<Job>>>) {
        let (runner, jobs, _) = Self::with_rewrites();
        (runner, jobs)
    }

    /// A runner that also hands back the rewrites it is asked for.
    pub fn with_rewrites() -> (Self, Arc<Mutex<Vec<Job>>>, Rewrites) {
        let jobs: Arc<Mutex<Vec<Job>>> = Default::default();
        let rewrites: Rewrites = Default::default();
        (Self { jobs: jobs.clone(), rewrites: rewrites.clone() }, jobs, rewrites)
    }
}

impl JobRunner for RecordingRunner {
    fn submit(&self, job: Job) {
        self.jobs.lock().unwrap().push(job);
    }

    fn cancel(&self, _id: &GenerationId) {}

    fn is_active(&self, id: &GenerationId) -> bool {
        self.jobs.lock().unwrap().iter().any(|job| job.id() == id)
    }

    fn active_count(&self) -> usize {
        self.jobs.lock().unwrap().len()
    }

    /// Records the ask and hands the test the sender: nothing answers until it decides to.
    fn improve_prompt(&self, request: TextRequest) -> ImproveReceiver {
        let (tx, rx) = improve_channel();
        self.rewrites.lock().unwrap().push((request, tx));
        rx
    }
}

/// "Relaunch": open the environment's library folder again over a [`RecordingRunner`], which
/// recovers the rows the first model left in flight, and make the new model the app's. Returns it
/// with the jobs the relaunch submitted.
pub fn reopen_recording(env: &TestEnv, cx: &mut TestAppContext) -> (Entity<LibraryModel>, Arc<Mutex<Vec<Job>>>) {
    let (runner, jobs) = RecordingRunner::new();
    let root = env.dir.path().to_path_buf();
    let library = cx.new(|cx| LibraryModel::open_with_runner(root, Box::new(runner), cx).unwrap());
    cx.update(|cx| {
        let keys = cx.global::<AppState>().keys.clone();
        cx.set_global(AppState { library: library.clone(), keys });
    });
    (library, jobs)
}

/// Convenience: run `setup` through a `TestAppContext`.
pub fn env(cx: &mut TestAppContext, images: usize, provider: &str) -> TestEnv {
    cx.update(|cx| setup(cx, images, provider))
}

pub fn env_with_keys(cx: &mut TestAppContext, images: usize, provider: &str, keys: ApiKeys) -> TestEnv {
    cx.update(|cx| setup_with_keys(cx, images, provider, keys))
}

/// In-memory [`SecretBackend`] that can be told to fail, shared by handle so tests can inspect what
/// was persisted after handing a clone to [`ApiKeys`].
#[derive(Clone, Default)]
pub struct TestBackend {
    keys: Arc<Mutex<KeyMap>>,
    fail_reads: Arc<AtomicBool>,
    /// Resolve reads a tick later, like the keychain does behind its dialog.
    slow_reads: Arc<AtomicBool>,
    fail_writes: Arc<AtomicBool>,
    writes: Arc<AtomicUsize>,
}

impl TestBackend {
    pub fn with<'a>(seed: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let keys = seed.into_iter().map(|(p, k)| (p.to_string(), k.to_string())).collect();
        Self { keys: Arc::new(Mutex::new(keys)), ..Default::default() }
    }

    pub fn fail_reads(&self, fail: bool) {
        self.fail_reads.store(fail, Ordering::SeqCst);
    }

    pub fn slow_reads(&self, slow: bool) {
        self.slow_reads.store(slow, Ordering::SeqCst);
    }

    pub fn fail_writes(&self, fail: bool) {
        self.fail_writes.store(fail, Ordering::SeqCst);
    }

    pub fn get(&self, provider: &str) -> Option<String> {
        self.keys.lock().unwrap().get(provider).cloned()
    }

    pub fn snapshot(&self) -> KeyMap {
        self.keys.lock().unwrap().clone()
    }

    /// Number of write attempts, failed ones included.
    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

impl SecretBackend for TestBackend {
    fn read(&self, cx: &mut App) -> Task<anyhow::Result<KeyMap>> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Task::ready(Err(anyhow!("read failed")));
        }
        let snapshot = self.snapshot();
        if self.slow_reads.load(Ordering::SeqCst) {
            let timer = cx.background_executor().timer(std::time::Duration::from_millis(10));
            return cx.background_spawn(async move {
                timer.await;
                Ok(snapshot)
            });
        }
        Task::ready(Ok(snapshot))
    }

    fn write(&self, keys: KeyMap, _cx: &mut App) -> Task<anyhow::Result<()>> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_writes.load(Ordering::SeqCst) {
            return Task::ready(Err(anyhow!("write failed")));
        }
        *self.keys.lock().unwrap() = keys;
        Task::ready(Ok(()))
    }
}

/// A library row to seed beyond the `env` images: any media type / status, with the flags the feed's
/// context menu keys off. Defaults to a completed, recreatable, un-favourited image.
#[derive(Clone, Copy)]
pub struct Seed {
    pub media_type: MediaType,
    pub status: Status,
    /// Stores a real Mock request so `Generation::can_recreate` / `can_retry` hold: an image
    /// generation, or with `upscaled` the Mock upscaler's tool request.
    pub recreatable: bool,
    /// Marks the row an upscaler output (`tool` set, `is_upscaled` true).
    pub upscaled: bool,
    pub favorite: bool,
    /// File contents to seed instead of the default (a solid PNG / the Mock clip); for corrupt or
    /// unsupported-codec cases.
    pub bytes: Option<&'static [u8]>,
}

impl Default for Seed {
    fn default() -> Self {
        Self { media_type: MediaType::Image, status: Status::Completed, recreatable: true, upscaled: false, favorite: false, bytes: None }
    }
}

/// A 64×64, 2 s, one-keyframe-per-second Mock clip, the same bytes the Mock provider returns,
/// encoded once per test binary.
pub fn mock_clip() -> &'static [u8] {
    static CLIP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    CLIP.get_or_init(|| majik_providers::mock::video_renderer::render_blocking(64, 64, 2, [200, 100, 50]).expect("mock clip encodes"))
}

/// [`mock_clip`] with its sample entry renamed to a codec nothing decodes.
pub fn unsupported_clip() -> &'static [u8] {
    static CLIP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    CLIP.get_or_init(|| {
        let mut clip = mock_clip().to_vec();
        let at = clip.windows(4).rposition(|w| w == b"avc1").expect("avc1 sample entry");
        clip[at..at + 4].copy_from_slice(b"zvc9");
        clip
    })
}

/// Import an asset of `kind` through the library model (so views observing it refresh): a solid
/// PNG, the Mock clip, or a stub WAV. `seed` varies the image so different seeds are different assets.
pub fn seed_asset(library: &Entity<LibraryModel>, cx: &mut TestAppContext, kind: MediaType, seed: u8) -> AssetId {
    library.update(cx, |model, cx| {
        let (content_type, bytes) = match kind {
            MediaType::Image => ("image/png", solid_png(5, 5, [seed, seed / 2, 255 - seed])),
            MediaType::Video => ("video/mp4", mock_clip().to_vec()),
            MediaType::Audio => ("audio/wav", format!("RIFF-stub-{seed}").into_bytes()),
        };
        model.import_asset(content_type, &bytes, cx).expect("asset imported")
    })
}

/// A completed row that stores `request` and references `inputs` as `(role raw, asset)`: what a
/// generation or a tool run leaves behind, for tests that recreate or retry it.
pub fn seed_request(library: &Entity<LibraryModel>, cx: &mut TestAppContext, request: &Request, inputs: &[(&str, AssetId)]) -> GenerationId {
    library.update(cx, |model, cx| {
        let id = model.lib.add_generating(request.media_type(), Some(request.to_json()), Some(request.generation_type.model_name().into()), Some(request.provider.to_string()), request.generation_type.tool());
        let links: Vec<(AssetId, &str)> = inputs.iter().map(|(role, asset)| (asset.clone(), *role)).collect();
        model.lib.attach_inputs(&id, &links).expect("inputs linked");
        let bytes = match request.media_type() {
            MediaType::Image => solid_png(32, 32, [200, 100, 50]),
            MediaType::Video => mock_clip().to_vec(),
            MediaType::Audio => b"not really media".to_vec(),
        };
        model.lib.complete_generation(&id, &bytes, request.generation_type.tool() == Some(ToolId::Upscale)).expect("completed");
        model.changed(cx);
        id
    })
}

/// Insert `seed` through the library model (so views observing it refresh) and return its id.
pub fn seed_item(library: &Entity<LibraryModel>, cx: &mut TestAppContext, seed: Seed) -> GenerationId {
    library.update(cx, |model, cx| {
        let request_json = seed.recreatable.then(|| {
            let generation_type = if seed.upscaled {
                GenerationType::for_tool_model(&catalog::tool::MOCK_UPSCALE)
            } else {
                GenerationType::Image(ImageGenerationSettings { model: catalog::image::ALL[0].clone(), aspect_ratio: AspectRatio::Square, resolution: ImageResolution::Sd })
            };
            let prompt = if seed.upscaled { "" } else { "seeded" };
            Request::new(ProviderId::mock(), generation_type, prompt, vec![]).to_json()
        });
        let tool = seed.upscaled.then_some(ToolId::Upscale);
        let id = model.lib.add_generating(seed.media_type, request_json, Some("mock".into()), Some("Mock".into()), tool);
        match seed.status {
            Status::Generating => {}
            Status::Completed => {
                let bytes = seed.bytes.map(<[u8]>::to_vec).unwrap_or_else(|| match seed.media_type {
                    MediaType::Image => solid_png(32, 32, [200, 100, 50]),
                    MediaType::Video => mock_clip().to_vec(),
                    MediaType::Audio => b"not really media".to_vec(),
                });
                model.lib.complete_generation(&id, &bytes, seed.upscaled).unwrap();
            }
            Status::Failed => model.lib.fail_generation(&id, "seeded failure"),
            Status::Missing => {
                // Generate the file, remove it behind the app's back, and reload as a relaunch would.
                let path = model.lib.complete_generation(&id, &solid_png(32, 32, [200, 100, 50]), seed.upscaled).unwrap();
                std::fs::remove_file(path).unwrap();
                model.lib.reload().unwrap();
            }
        }
        if seed.favorite {
            model.lib.set_favorite(&id, true);
        }
        model.changed(cx);
        id
    })
}

/// Stands in for `LibraryWindow` for a picker's tests: the composer plus the dialog layer, inside
/// a `Root`, so a picker opened over it is actually drawn and its key handlers are on the focus path.
struct ComposeHost {
    compose: Entity<crate::views::compose::ComposeView>,
}

impl gpui::Render for ComposeHost {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _};
        gpui::div().size_full().child(self.compose.clone()).children(gpui_component::Root::render_dialog_layer(window, cx))
    }
}

/// A seeded Mock environment and a composer drawn under a dialog layer; see [`ComposeHost`].
/// The environment is returned so the caller keeps the temp library alive for the test.
pub fn compose_with_dialogs(cx: &mut TestAppContext) -> (Entity<crate::views::compose::ComposeView>, &mut gpui::VisualTestContext, TestEnv) {
    let env = env(cx, 1, "Mock");
    let slot: std::rc::Rc<std::cell::RefCell<Option<Entity<crate::views::compose::ComposeView>>>> = Default::default();
    let slot_for_window = slot.clone();
    let (_root, vcx) = cx.add_window_view(move |window, cx| {
        let compose = cx.new(|cx| crate::views::compose::ComposeView::new(window, cx));
        *slot_for_window.borrow_mut() = Some(compose.clone());
        let host = cx.new(|_| ComposeHost { compose });
        gpui_component::Root::new(gpui::AnyView::from(host), window, cx)
    });
    vcx.run_until_parked();
    let view = slot.borrow().clone().unwrap();
    (view, vcx, env)
}
