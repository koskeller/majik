//! Usage events, the way Zed's `client::telemetry` does them. Every crate fires
//! `majik_telemetry::event!`; this module receives those events, drops them while the user has
//! usage data off, queues the rest, and posts a batch to `<base>/events` every
//! [`FLUSH_INTERVAL`] or once [`MAX_QUEUE_LEN`] have piled up. Every batch is also appended, one
//! JSON line per event, to `telemetry.log` in the logs folder, which is what the Settings window's
//! Telemetry page shows: the user can read exactly what left the machine.
//!
//! Sending goes through a [`Transport`] so tests record instead of posting. The HTTP transport
//! signs each body with `x-majik-checksum` (SHA-256 over `seed ‖ body ‖ seed`, the seed baked in
//! by the release scripts), which is how the server tells an official build from anything else.
//! Crash reports (`reliability.rs`) use the same transport for `<base>/crashes`.

use crate::config::{self, Config, TelemetrySettings};
use futures::channel::mpsc;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, BackgroundExecutor, Task};
use majik_telemetry::{EventRequestBody, EventWrapper, FlexibleEvent};
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
pub const MAX_QUEUE_LEN: usize = 5;
#[cfg(not(debug_assertions))]
pub const MAX_QUEUE_LEN: usize = 50;

#[cfg(debug_assertions)]
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(not(debug_assertions))]
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60 * 5);

/// How much of `telemetry.log` the Telemetry page reads back, from the end.
const MAX_LOG_READ: usize = 5 * 1024 * 1024;

/// One request to the telemetry server.
#[derive(Clone, Debug, PartialEq)]
pub enum TelemetryRequest {
    /// `POST <base>/events`: a serialised [`EventRequestBody`].
    Events(Vec<u8>),
    /// `POST <base>/crashes`: a `majik_crashes::CrashInfo` and the zstd-compressed minidump.
    Crash { metadata: Vec<u8>, minidump: Vec<u8> },
}

/// Where requests go. Blocking: the app calls it from the background executor.
pub trait Transport: Send + Sync {
    fn send(&self, request: TelemetryRequest) -> anyhow::Result<()>;
}

/// The real thing: `POST`s to the base URL with the checksum header.
pub struct HttpTransport {
    base_url: String,
    seed: Option<Vec<u8>>,
}

impl HttpTransport {
    pub fn new(base_url: String, seed: Option<Vec<u8>>) -> Self {
        Self { base_url, seed }
    }

    fn client() -> anyhow::Result<&'static reqwest::blocking::Client> {
        static CLIENT: std::sync::OnceLock<Result<reqwest::blocking::Client, String>> = std::sync::OnceLock::new();
        CLIENT
            .get_or_init(|| reqwest::blocking::Client::builder().user_agent(concat!("majik/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("building the telemetry client: {e}"))
    }

    fn signed(&self, request: reqwest::blocking::RequestBuilder, body: &[u8]) -> reqwest::blocking::RequestBuilder {
        match self.seed.as_deref().map(|seed| calculate_json_checksum(seed, body)) {
            Some(checksum) => request.header("x-majik-checksum", checksum),
            None => request,
        }
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: TelemetryRequest) -> anyhow::Result<()> {
        let response = match request {
            TelemetryRequest::Events(body) => {
                let request = Self::client()?.post(format!("{}/events", self.base_url)).header("Content-Type", "application/json");
                self.signed(request, &body).body(body).send()?
            }
            TelemetryRequest::Crash { metadata, minidump } => {
                let form = reqwest::blocking::multipart::Form::new()
                    .part("metadata", reqwest::blocking::multipart::Part::bytes(metadata.clone()).mime_str("application/json")?)
                    .part("minidump", reqwest::blocking::multipart::Part::bytes(minidump).file_name("minidump.dmp").mime_str("application/octet-stream")?);
                let request = Self::client()?.post(format!("{}/crashes", self.base_url));
                self.signed(request, &metadata).multipart(form).send()?
            }
        };
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("the telemetry server answered {status}: {}", response.text().unwrap_or_default());
        }
        Ok(())
    }
}

/// A build with nowhere to send: events still reach the log, so the Telemetry page works the
/// same. A crash report is refused rather than swallowed, so it stays on disk instead of being
/// deleted as "sent".
pub struct NoTransport;

impl Transport for NoTransport {
    fn send(&self, request: TelemetryRequest) -> anyhow::Result<()> {
        match request {
            TelemetryRequest::Events(_) => Ok(()),
            TelemetryRequest::Crash { .. } => anyhow::bail!("this build has no telemetry endpoint"),
        }
    }
}

/// The transport `main` picks for this build: HTTP when there is a base URL, else nothing.
pub fn transport_for_build() -> Arc<dyn Transport> {
    match config::telemetry_base_url() {
        Some(base_url) => {
            let seed = config::telemetry_seed();
            if seed.is_none() {
                tracing::warn!(target: "majik", "telemetry is unsigned: no MAJIK_TELEMETRY_SEED in this build or environment");
            }
            Arc::new(HttpTransport::new(base_url, seed))
        }
        None => Arc::new(NoTransport),
    }
}

/// Hex SHA-256 of `seed ‖ body ‖ seed`, Zed's `calculate_json_checksum`.
pub fn calculate_json_checksum(seed: &[u8], body: &[u8]) -> String {
    let mut summer = Sha256::new();
    summer.update(seed);
    summer.update(body);
    summer.update(seed);
    summer.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether events fired from this thread or from the whole process reach this instance; the app
/// uses `Process`, tests `Thread` so each test's events stay with its own instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Process,
    #[cfg_attr(not(test), allow(dead_code))]
    Thread,
}

pub struct Telemetry {
    executor: BackgroundExecutor,
    transport: Arc<dyn Transport>,
    state: Arc<Mutex<TelemetryState>>,
}

struct TelemetryState {
    settings: TelemetrySettings,
    installation_id: Option<String>,
    session_id: String,
    session_started: Instant,
    app_version: String,
    os_name: String,
    os_version: Option<String>,
    architecture: &'static str,
    release_channel: &'static str,
    queue: Vec<EventWrapper>,
    first_event_at: Option<Instant>,
    flush_task: Option<Task<()>>,
    max_queue_len: usize,
    /// `telemetry.log`, appended to; every flushed event is one line.
    log_file: Option<File>,
    log_path: Option<PathBuf>,
    /// The Telemetry page, while it is open.
    subscribers: Vec<mpsc::UnboundedSender<EventWrapper>>,
}

/// What the Telemetry page starts from: the log's events, then what is queued, then a live feed.
pub struct TelemetrySubscription {
    pub queued: Vec<EventWrapper>,
    pub live: mpsc::UnboundedReceiver<EventWrapper>,
}

/// `telemetry.log` read back: its events and how many lines were not one.
#[derive(Debug, Default, PartialEq)]
pub struct HistoricalEvents {
    pub events: Vec<EventWrapper>,
    pub parse_error_count: usize,
}

impl Telemetry {
    /// Start receiving events. `installation_id` is `None` only when the config could not be
    /// written, in which case nothing is ever sent (the queue still fills the log).
    pub fn new(transport: Arc<dyn Transport>, installation_id: Option<String>, session_id: String, route: Route, cx: &mut App) -> Arc<Self> {
        let log_path = config::logs_dir().map(|dir| dir.join("telemetry.log"));
        let log_file = log_path.as_ref().and_then(|path| match open_log(path) {
            Ok(file) => Some(file),
            Err(e) => {
                tracing::warn!(target: "majik", "opening {}: {e}", path.display());
                None
            }
        });
        let os_name = majik_platform::system::os_name();
        let state = Arc::new(Mutex::new(TelemetryState {
            settings: cx.global::<Config>().telemetry,
            installation_id,
            session_id,
            session_started: Instant::now(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            os_name,
            os_version: None,
            architecture: std::env::consts::ARCH,
            release_channel: config::channel().name(),
            queue: Vec::new(),
            first_event_at: None,
            flush_task: None,
            max_queue_len: MAX_QUEUE_LEN,
            log_file,
            log_path,
            subscribers: Vec::new(),
        }));
        let this = Arc::new(Self { executor: cx.background_executor().clone(), transport, state });

        // Asking the OS its version can block, so it happens off the UI thread.
        cx.background_spawn({
            let state = this.state.clone();
            async move {
                let version = majik_platform::system::os_version();
                lock(&state).os_version = Some(version);
            }
        })
        .detach();

        cx.observe_global::<Config>({
            let state = this.state.clone();
            move |cx| lock(&state).settings = cx.global::<Config>().telemetry
        })
        .detach();

        let (tx, mut rx) = mpsc::unbounded();
        match route {
            Route::Process => majik_telemetry::init(tx),
            Route::Thread => majik_telemetry::init_for_thread(Some(tx)),
        }
        cx.background_spawn({
            let this = Arc::downgrade(&this);
            async move {
                while let Some(event) = rx.next().await {
                    let Some(this) = this.upgrade() else { break };
                    this.report_event(event);
                }
            }
        })
        .detach();

        this
    }

    pub fn settings(&self) -> TelemetrySettings {
        lock(&self.state).settings
    }

    /// The config observer applies a change at the next effect flush; a caller that needs the
    /// new value honoured for the very next event sets it here first.
    pub fn set_settings(&self, settings: TelemetrySettings) {
        lock(&self.state).settings = settings;
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        lock(&self.state).log_path.clone()
    }

    #[cfg(test)]
    pub fn set_max_queue_len(&self, len: usize) {
        lock(&self.state).max_queue_len = len;
    }

    /// The events waiting to be sent, for tests.
    #[cfg(test)]
    pub fn queued_events(&self) -> Vec<FlexibleEvent> {
        lock(&self.state).queue.iter().map(|wrapper| wrapper.event.clone()).collect()
    }

    /// Queue one event: what the process-wide receiver does with everything `event!` fires.
    pub fn report_event(self: &Arc<Self>, mut event: FlexibleEvent) {
        let mut state = lock(&self.state);
        tracing::trace!(target: "telemetry", "{event:?}");
        if !state.settings.metrics {
            return;
        }
        event.event_properties.insert("event_source".into(), "majik".into());

        if state.flush_task.is_none() {
            let this = self.clone();
            state.flush_task = Some(self.executor.spawn(async move {
                this.executor.timer(FLUSH_INTERVAL).await;
                this.flush_events().detach();
            }));
        }

        let now = Instant::now();
        let milliseconds_since_first_event = match state.first_event_at {
            Some(first) => now.saturating_duration_since(first).min(Duration::from_secs(60 * 60 * 24)).as_millis() as i64,
            None => {
                state.first_event_at = Some(now);
                0
            }
        };
        let wrapper = EventWrapper { milliseconds_since_first_event, event };
        state.subscribers.retain(|tx| tx.unbounded_send(wrapper.clone()).is_ok());
        state.queue.push(wrapper);

        if state.installation_id.is_some() && state.queue.len() >= state.max_queue_len {
            drop(state);
            self.flush_events().detach();
        }
    }

    /// Send what is queued, on the background executor.
    pub fn flush_events(self: &Arc<Self>) -> Task<()> {
        let this = self.clone();
        self.executor.spawn(async move {
            if let Err(e) = this.flush_events_inner() {
                tracing::warn!(target: "majik", "sending telemetry: {e:#}");
            }
        })
    }

    /// Write the queue to the log and post it. Blocking (the HTTP call); called off the UI thread.
    fn flush_events_inner(&self) -> anyhow::Result<()> {
        let body = {
            let mut state = lock(&self.state);
            state.first_event_at = None;
            state.flush_task.take();
            let events = std::mem::take(&mut state.queue);
            if events.is_empty() {
                return Ok(());
            }
            if let Some(file) = &mut state.log_file {
                for event in &events {
                    let mut line = serde_json::to_vec(event)?;
                    line.push(b'\n');
                    file.write_all(&line)?;
                }
            }
            EventRequestBody {
                installation_id: state.installation_id.clone(),
                session_id: Some(state.session_id.clone()),
                app_version: state.app_version.clone(),
                os_name: state.os_name.clone(),
                os_version: state.os_version.clone(),
                architecture: state.architecture.to_string(),
                release_channel: Some(state.release_channel.to_string()),
                events,
            }
        };
        self.transport.send(TelemetryRequest::Events(serde_json::to_vec(&body)?))
    }

    /// Send a crash report through the same transport; `reliability.rs` calls this off the UI
    /// thread with the files it found.
    pub fn send_crash(&self, metadata: Vec<u8>, minidump: Vec<u8>) -> anyhow::Result<()> {
        self.transport.send(TelemetryRequest::Crash { metadata, minidump })
    }

    /// "App Closed", then a best-effort flush. gpui gives quit observers 200 ms, so a slow
    /// network loses this batch; the periodic flush bounds what a lost batch can hold.
    pub fn shutdown(self: &Arc<Self>) -> Task<()> {
        let session_seconds = lock(&self.state).session_started.elapsed().as_secs();
        self.report_event(FlexibleEvent {
            event_type: "App Closed".into(),
            event_properties: [("session_seconds".to_string(), session_seconds.into())].into(),
        });
        self.flush_events()
    }

    /// What is queued now plus a feed of everything queued from now on, for the Telemetry page.
    pub fn subscribe(&self) -> TelemetrySubscription {
        let mut state = lock(&self.state);
        let (tx, rx) = mpsc::unbounded();
        state.subscribers.push(tx);
        TelemetrySubscription { queued: state.queue.clone(), live: rx }
    }

    /// Read the log back (the last [`MAX_LOG_READ`] bytes). Blocking IO: call it off the UI thread.
    pub fn read_log(&self) -> anyhow::Result<HistoricalEvents> {
        let Some(path) = self.log_path() else { return Ok(HistoricalEvents::default()) };
        Ok(parse_log(&std::fs::read(&path)?))
    }

    /// Empty the log: the Telemetry page's Clear.
    pub fn clear_log(&self) -> anyhow::Result<()> {
        let mut state = lock(&self.state);
        if let Some(path) = state.log_path.clone() {
            state.log_file = Some(File::create(path)?);
        }
        Ok(())
    }
}

/// Open `telemetry.log` for appending. Two instances of a channel (routine for Dev) share it, so
/// it is never truncated, only moved aside as `telemetry.log.old` once it is past what the
/// Telemetry page reads back.
fn open_log(path: &std::path::Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).map(|m| m.len() as usize >= MAX_LOG_READ).unwrap_or(false) {
        std::fs::rename(path, path.with_extension("log.old"))?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// The events in a `telemetry.log`, from its last [`MAX_LOG_READ`] bytes (whole lines only).
fn parse_log(content: &[u8]) -> HistoricalEvents {
    let start = if content.len() > MAX_LOG_READ {
        let skip = content.len() - MAX_LOG_READ;
        content[skip..].iter().position(|&b| b == b'\n').map(|pos| skip + pos + 1).unwrap_or(skip)
    } else {
        0
    };
    let mut historical = HistoricalEvents::default();
    for line in String::from_utf8_lossy(&content[start..]).lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<EventWrapper>(line) {
            Ok(event) => historical.events.push(event),
            Err(_) => historical.parse_error_count += 1,
        }
    }
    historical
}

fn lock(state: &Mutex<TelemetryState>) -> MutexGuard<'_, TelemetryState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub mod test_transport {
    use super::*;

    /// Records what would have been sent; `fail` makes every send fail.
    #[derive(Default)]
    pub struct RecordingTransport {
        pub requests: Mutex<Vec<TelemetryRequest>>,
        pub fail: std::sync::atomic::AtomicBool,
    }

    impl RecordingTransport {
        pub fn requests(&self) -> Vec<TelemetryRequest> {
            self.requests.lock().unwrap().clone()
        }

        /// The bodies of the event batches sent so far, parsed.
        pub fn batches(&self) -> Vec<EventRequestBody> {
            self.requests()
                .into_iter()
                .filter_map(|request| match request {
                    TelemetryRequest::Events(body) => serde_json::from_slice(&body).ok(),
                    TelemetryRequest::Crash { .. } => None,
                })
                .collect()
        }

        /// Every event sent so far, in order.
        pub fn sent_events(&self) -> Vec<FlexibleEvent> {
            self.batches().into_iter().flat_map(|batch| batch.events.into_iter().map(|wrapper| wrapper.event)).collect()
        }
    }

    impl Transport for RecordingTransport {
        fn send(&self, request: TelemetryRequest) -> anyhow::Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("the telemetry server is down");
            }
            self.requests.lock().unwrap().push(request);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_transport::RecordingTransport;
    use super::*;
    use crate::test_support::env;
    use gpui::TestAppContext;

    fn telemetry(cx: &mut TestAppContext, transport: Arc<RecordingTransport>) -> Arc<Telemetry> {
        cx.update(|cx| Telemetry::new(transport, Some("install-1".into()), "session-1".into(), Route::Thread, cx))
    }

    fn event(name: &str) -> FlexibleEvent {
        FlexibleEvent { event_type: name.into(), event_properties: [("key".to_string(), "value".into())].into() }
    }

    #[gpui::test]
    fn events_flush_once_the_queue_is_full(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let transport = Arc::new(RecordingTransport::default());
        let telemetry = telemetry(cx, transport.clone());
        telemetry.set_max_queue_len(3);
        telemetry.report_event(event("One"));
        telemetry.report_event(event("Two"));
        cx.run_until_parked();
        assert_eq!(telemetry.queued_events().len(), 2);
        assert!(transport.requests().is_empty(), "nothing sent before the queue is full");
        telemetry.report_event(event("Three"));
        cx.run_until_parked();
        assert!(telemetry.queued_events().is_empty(), "the queue emptied into one request");
        let batches = transport.batches();
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        assert_eq!(batch.installation_id.as_deref(), Some("install-1"));
        assert_eq!(batch.session_id.as_deref(), Some("session-1"));
        assert_eq!(batch.release_channel.as_deref(), Some("dev"));
        assert_eq!(batch.app_version, env!("CARGO_PKG_VERSION"));
        assert!(batch.os_version.is_some(), "the OS version was read on the background executor");
        assert!(!batch.os_name.is_empty());
        assert_eq!(batch.architecture, std::env::consts::ARCH);
        let names: Vec<&str> = batch.events.iter().map(|e| e.event.event_type.as_str()).collect();
        assert_eq!(names, ["One", "Two", "Three"]);
        assert_eq!(batch.events[0].milliseconds_since_first_event, 0);
        assert_eq!(batch.events[0].event.event_properties["event_source"], "majik", "every event says which app sent it");
        assert_eq!(batch.events[0].event.event_properties["key"], "value");
    }

    #[gpui::test]
    fn events_flush_on_the_interval(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let transport = Arc::new(RecordingTransport::default());
        let telemetry = telemetry(cx, transport.clone());
        telemetry.report_event(event("Only"));
        cx.background_executor.advance_clock(FLUSH_INTERVAL - Duration::from_millis(1));
        cx.run_until_parked();
        assert_eq!(telemetry.queued_events().len(), 1, "a millisecond early, still queued");
        cx.background_executor.advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(telemetry.queued_events().is_empty());
        assert_eq!(transport.sent_events().len(), 1);
        // The next event arms the timer again rather than riding the old one.
        telemetry.report_event(event("Later"));
        cx.background_executor.advance_clock(FLUSH_INTERVAL);
        cx.run_until_parked();
        assert_eq!(transport.sent_events().len(), 2);
    }

    #[gpui::test]
    fn the_macro_reaches_the_instance_through_the_thread_route(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        majik_telemetry::event!("Fired From A View", model = "flux");
        cx.run_until_parked();
        let queued = e.telemetry.queued_events();
        assert_eq!(queued.len(), 1, "{queued:?}");
        assert_eq!(queued[0].event_type, "Fired From A View");
        assert_eq!(queued[0].event_properties["model"], "flux");
    }

    #[gpui::test]
    fn metrics_off_drops_events_and_on_again_takes_them(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        cx.update(|cx| crate::config::update_config(cx, |c| c.telemetry.metrics = false));
        cx.run_until_parked();
        assert!(!e.telemetry.settings().metrics, "the instance follows the config");
        majik_telemetry::event!("While Off");
        cx.run_until_parked();
        assert!(e.telemetry.queued_events().is_empty(), "dropped, not queued");
        cx.update(|cx| crate::config::update_config(cx, |c| c.telemetry.metrics = true));
        majik_telemetry::event!("While On");
        cx.run_until_parked();
        assert_eq!(e.telemetry.queued_events().len(), 1);
    }

    #[gpui::test]
    fn a_failed_send_is_logged_and_the_batch_is_gone(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let transport = Arc::new(RecordingTransport::default());
        transport.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let telemetry = telemetry(cx, transport.clone());
        telemetry.set_max_queue_len(1);
        telemetry.report_event(event("Lost"));
        cx.run_until_parked();
        // Zed's behaviour too: telemetry never retries, so an outage costs a batch, not memory.
        assert!(telemetry.queued_events().is_empty());
        assert!(transport.requests().is_empty());
    }

    #[gpui::test]
    fn without_an_installation_id_nothing_is_sent_but_the_queue_still_flushes_to_the_log(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let transport = Arc::new(RecordingTransport::default());
        let telemetry = cx.update(|cx| Telemetry::new(transport.clone(), None, "session-1".into(), Route::Thread, cx));
        telemetry.set_max_queue_len(1);
        telemetry.report_event(event("Anonymous"));
        cx.run_until_parked();
        assert_eq!(telemetry.queued_events().len(), 1, "a full queue does not flush without an id");
        cx.background_executor.advance_clock(FLUSH_INTERVAL);
        cx.run_until_parked();
        let batches = transport.batches();
        assert_eq!(batches.len(), 1, "the timer still flushes");
        assert_eq!(batches[0].installation_id, None);
    }

    #[gpui::test]
    fn subscribers_see_queued_and_live_events(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let transport = Arc::new(RecordingTransport::default());
        let telemetry = telemetry(cx, transport);
        telemetry.report_event(event("Before"));
        let mut subscription = telemetry.subscribe();
        assert_eq!(subscription.queued.len(), 1);
        telemetry.report_event(event("After"));
        let live = subscription.live.try_recv().expect("a live event");
        assert_eq!(live.event.event_type, "After");
        drop(subscription);
        telemetry.report_event(event("Nobody Listening"));
        assert_eq!(telemetry.queued_events().len(), 3, "a gone subscriber is dropped, the event is kept");
    }

    #[gpui::test]
    fn shutdown_fires_app_closed_and_flushes(cx: &mut TestAppContext) {
        let _e = env(cx, 0, "Mock");
        let transport = Arc::new(RecordingTransport::default());
        let telemetry = telemetry(cx, transport.clone());
        telemetry.report_event(event("Something"));
        let task = telemetry.shutdown();
        cx.run_until_parked();
        drop(task);
        let sent = transport.sent_events();
        assert_eq!(sent.iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>(), ["Something", "App Closed"]);
        assert!(sent[1].event_properties["session_seconds"].is_u64());
    }

    /// A one-request HTTP server on a local port: what it received, verbatim.
    fn serve_once() -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let n = stream.read(&mut buffer).unwrap();
                received.extend_from_slice(&buffer[..n]);
                let head_end = received.windows(4).position(|w| w == b"\r\n\r\n");
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&received[..head_end]).to_lowercase();
                let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length: ")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                if received.len() >= head_end + 4 + length {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            received
        });
        (url, handle)
    }

    #[test]
    fn the_http_transport_posts_events_with_the_checksum_header() {
        let (url, server) = serve_once();
        let transport = HttpTransport::new(url, Some(b"seed".to_vec()));
        transport.send(TelemetryRequest::Events(b"body".to_vec())).unwrap();
        let received = String::from_utf8_lossy(&server.join().unwrap()).to_string();
        assert!(received.starts_with("POST /events HTTP/1.1"), "{received}");
        assert!(received.to_lowercase().contains("content-type: application/json"), "{received}");
        assert!(received.contains(&format!("x-majik-checksum: {}", calculate_json_checksum(b"seed", b"body"))), "{received}");
        assert!(received.ends_with("body"), "{received}");
    }

    #[test]
    fn the_http_transport_posts_crashes_as_multipart_signed_over_the_metadata() {
        let (url, server) = serve_once();
        let transport = HttpTransport::new(url, Some(b"seed".to_vec()));
        transport.send(TelemetryRequest::Crash { metadata: b"{\"panic\":1}".to_vec(), minidump: b"MDMP".to_vec() }).unwrap();
        let received = String::from_utf8_lossy(&server.join().unwrap()).to_string();
        assert!(received.starts_with("POST /crashes HTTP/1.1"), "{received}");
        assert!(received.to_lowercase().contains("content-type: multipart/form-data; boundary="), "{received}");
        assert!(received.contains(&format!("x-majik-checksum: {}", calculate_json_checksum(b"seed", b"{\"panic\":1}"))), "{received}");
        assert!(received.contains("name=\"metadata\"") && received.contains("{\"panic\":1}"), "{received}");
        assert!(received.contains("name=\"minidump\"; filename=\"minidump.dmp\"") && received.contains("MDMP"), "{received}");
    }

    #[test]
    fn the_http_transport_reports_a_rejection() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            // Whatever arrived is enough: the answer does not depend on the request.
            let _ = stream.read(&mut buffer).unwrap();
            stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 12\r\nConnection: close\r\n\r\nbad checksum").unwrap();
        });
        let transport = HttpTransport::new(url, None);
        let error = transport.send(TelemetryRequest::Events(b"body".to_vec())).unwrap_err().to_string();
        assert!(error.contains("403") && error.contains("bad checksum"), "{error}");
    }

    #[test]
    fn a_build_without_an_endpoint_logs_events_but_refuses_crash_reports() {
        // Events have the log to land in; a crash report "sent" nowhere would be deleted.
        assert!(NoTransport.send(TelemetryRequest::Events(b"{}".to_vec())).is_ok());
        assert!(NoTransport.send(TelemetryRequest::Crash { metadata: vec![], minidump: vec![] }).is_err());
    }

    #[test]
    fn the_checksum_is_sha256_of_seed_body_seed() {
        // `printf 'seedbodyseed' | shasum -a 256`
        assert_eq!(calculate_json_checksum(b"seed", b"body"), "4efb65bc417a61cd876ee8416c9a12c8d54131282054a44438199456d49af75b");
        assert_ne!(calculate_json_checksum(b"other", b"body"), calculate_json_checksum(b"seed", b"body"), "the seed is part of it");
    }

    #[test]
    fn the_log_reads_back_and_skips_lines_that_are_not_events() {
        let wrapper = EventWrapper { milliseconds_since_first_event: 5, event: event("Logged") };
        let line = serde_json::to_string(&wrapper).unwrap();
        let content = format!("{line}\nnot json\n\n{line}\n");
        let historical = parse_log(content.as_bytes());
        assert_eq!(historical.events, vec![wrapper.clone(), wrapper.clone()]);
        assert_eq!(historical.parse_error_count, 1);
        // A log past the read limit is read from its tail, whole lines only.
        let mut big = vec![b'x'; MAX_LOG_READ];
        big.extend_from_slice(format!("\n{line}\n").as_bytes());
        let historical = parse_log(&big);
        assert_eq!(historical.events, vec![wrapper]);
        assert_eq!(historical.parse_error_count, 0, "the cut line is skipped rather than counted");
    }
}
