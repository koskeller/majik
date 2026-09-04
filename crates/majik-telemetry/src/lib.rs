//! Telemetry events, the way Zed's `telemetry` and `telemetry_events` crates do them: any crate
//! fires an event with [`event!`], and whoever called [`init`] (the app's `telemetry` module)
//! receives it on a channel, decides whether the user allows sending it, queues it, and posts a
//! batch every few minutes. This crate knows nothing about settings, files or HTTP, so it can sit
//! under every other crate.
//!
//! **What an event may carry.** A name in "Noun Verbed" form and properties that describe *what
//! kind* of thing happened: a provider or model name, a media type, a count, a duration, an error
//! *variant*. Never a prompt, a file name or path, an asset or generation id, a provider's error
//! message, an API key, or anything else the user typed or the library holds. Review every
//! `event!` against that list; the app's docs (`docs/telemetry.md`) promise it.
//!
//! `RUST_LOG=telemetry=trace` prints every event as the app queues it.

use futures::channel::mpsc;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub use serde_json;

/// Fire a telemetry event. By convention the name is "Noun Verbed" ("Generation Requested"), and
/// every property is anything `serde::Serialize` names a value for.
///
/// ```
/// let model = "flux";
/// majik_telemetry::event!("App Opened");
/// majik_telemetry::event!("Generation Requested", model, media_type = "image", batch = 2);
/// ```
#[macro_export]
macro_rules! event {
    ($name:expr) => {{
        $crate::send_event($crate::FlexibleEvent { event_type: $name.to_string(), event_properties: ::std::collections::BTreeMap::new() });
    }};
    ($name:expr, $($key:ident $(= $value:expr)?),+ $(,)?) => {{
        let event_properties = ::std::collections::BTreeMap::from([
            $(
                (
                    stringify!($key).to_string(),
                    $crate::serde_json::to_value(&$crate::property!($key $(= $value)?)).unwrap_or($crate::serde_json::Value::Null),
                ),
            )+
        ]);
        $crate::send_event($crate::FlexibleEvent { event_type: $name.to_string(), event_properties });
    }};
}

/// `key` alone means `key = key`, as in Zed.
#[doc(hidden)]
#[macro_export]
macro_rules! property {
    ($key:ident) => {
        $key
    };
    ($key:ident = $value:expr) => {
        $value
    };
}

/// One event: its name and its properties.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlexibleEvent {
    pub event_type: String,
    pub event_properties: BTreeMap<String, serde_json::Value>,
}

/// An event as it sits in the queue, the log and the request: the event plus its offset from the
/// batch's first event, which is how the server dates it without trusting the client's clock.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventWrapper {
    /// Milliseconds between this event and the first event of the batch it was sent in.
    pub milliseconds_since_first_event: i64,
    #[serde(flatten)]
    pub event: FlexibleEvent,
}

/// The body of `POST <base>/events`. Zed's `EventRequestBody` without the fields Majik has no
/// source for: there are no accounts (`metrics_id`, `is_staff`) and no machine-wide id
/// (`system_id`; the two channels are independent installs and only Stable sends).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRequestBody {
    /// Random per install (`config.json`), the only identity an event carries.
    pub installation_id: Option<String>,
    /// Random per launch.
    pub session_id: Option<String>,
    pub app_version: String,
    pub os_name: String,
    pub os_version: Option<String>,
    pub architecture: String,
    /// `"stable"` or `"dev"`.
    pub release_channel: Option<String>,
    pub events: Vec<EventWrapper>,
}

static QUEUE: OnceLock<mpsc::UnboundedSender<FlexibleEvent>> = OnceLock::new();

thread_local! {
    static THREAD_QUEUE: RefCell<Option<mpsc::UnboundedSender<FlexibleEvent>>> = const { RefCell::new(None) };
}

/// Hand every event fired from now on to `tx`. Called once by the app; a second call is ignored.
pub fn init(tx: mpsc::UnboundedSender<FlexibleEvent>) {
    QUEUE.set(tx).ok();
}

/// Hand every event fired *from this thread* to `tx`, ahead of the process-wide receiver. For
/// tests: gpui's test executor runs foreground and background work on the test's own thread, so
/// each test can have its own receiver while tests run in parallel. Clearing with `None` restores
/// the process-wide receiver.
pub fn init_for_thread(tx: Option<mpsc::UnboundedSender<FlexibleEvent>>) {
    THREAD_QUEUE.with(|queue| *queue.borrow_mut() = tx);
}

/// What [`event!`] expands to; call it directly when the event is built at runtime.
pub fn send_event(event: FlexibleEvent) {
    let sent = THREAD_QUEUE.with(|queue| queue.borrow().as_ref().map(|tx| tx.unbounded_send(event.clone()).is_ok()));
    match sent {
        // A thread-local receiver takes the event whether or not it is still listening; the
        // process-wide one only sees events from threads that never registered their own.
        Some(_) => {}
        None => {
            if let Some(queue) = QUEUE.get() {
                queue.unbounded_send(event).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::UnboundedReceiver<FlexibleEvent>) -> Vec<FlexibleEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn the_macro_builds_an_event_with_and_without_properties() {
        let (tx, mut rx) = mpsc::unbounded();
        init_for_thread(Some(tx));
        let model = "flux";
        event!("App Opened");
        event!("Generation Requested", model, media_type = "image", batch = 2, tool = Option::<&str>::None);
        init_for_thread(None);
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], FlexibleEvent { event_type: "App Opened".into(), event_properties: BTreeMap::new() });
        assert_eq!(events[1].event_type, "Generation Requested");
        assert_eq!(events[1].event_properties["model"], "flux", "a bare identifier is `key = key`");
        assert_eq!(events[1].event_properties["media_type"], "image");
        assert_eq!(events[1].event_properties["batch"], 2);
        assert_eq!(events[1].event_properties["tool"], serde_json::Value::Null);
    }

    #[test]
    fn events_go_nowhere_until_a_receiver_is_installed() {
        // No panic and no leak: a crate below the app may fire before the app is up.
        init_for_thread(None);
        event!("Fired Into The Void");
    }

    #[test]
    fn the_thread_receiver_takes_precedence_and_the_process_receiver_is_set_once() {
        let (process_tx, mut process_rx) = mpsc::unbounded();
        init(process_tx);
        let (later_tx, mut later_rx) = mpsc::unbounded();
        init(later_tx);
        let (thread_tx, mut thread_rx) = mpsc::unbounded();
        init_for_thread(Some(thread_tx));
        event!("Seen By The Thread");
        init_for_thread(None);
        event!("Seen By The Process");
        assert_eq!(drain(&mut thread_rx).iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>(), ["Seen By The Thread"]);
        let process: Vec<String> = drain(&mut process_rx).into_iter().map(|e| e.event_type).collect();
        // Other tests in this process may have fired into the same receiver; ours is among them.
        assert!(process.contains(&"Seen By The Process".to_string()), "{process:?}");
        assert!(!process.contains(&"Seen By The Thread".to_string()));
        // `try_recv` is `Err` while the channel is open and empty: nothing was routed to it.
        assert!(later_rx.try_recv().is_err(), "the second init is ignored");
    }

    /// The backend contract: this is the JSON `POST <base>/events` carries.
    #[test]
    fn the_request_body_serialises_to_the_documented_shape() {
        let body = EventRequestBody {
            installation_id: Some("install-1".into()),
            session_id: Some("session-1".into()),
            app_version: "0.1.0".into(),
            os_name: "macOS".into(),
            os_version: Some("15.6.1".into()),
            architecture: "aarch64".into(),
            release_channel: Some("stable".into()),
            events: vec![EventWrapper {
                milliseconds_since_first_event: 250,
                event: FlexibleEvent {
                    event_type: "Generation Requested".into(),
                    event_properties: BTreeMap::from([("model".to_string(), "flux".into()), ("batch".to_string(), 2.into())]),
                },
            }],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "installation_id": "install-1",
                "session_id": "session-1",
                "app_version": "0.1.0",
                "os_name": "macOS",
                "os_version": "15.6.1",
                "architecture": "aarch64",
                "release_channel": "stable",
                "events": [{
                    "milliseconds_since_first_event": 250,
                    "event_type": "Generation Requested",
                    "event_properties": { "batch": 2, "model": "flux" }
                }]
            })
        );
        let back: EventRequestBody = serde_json::from_value(json).unwrap();
        assert_eq!(back, body, "and the log's lines read back the same way");
    }

    #[test]
    fn the_thread_receiver_survives_the_receiving_end_closing() {
        // A test's receiver dropping must not make later events fall through to the process-wide
        // receiver of some other test.
        let (thread_tx, thread_rx) = mpsc::unbounded();
        init_for_thread(Some(thread_tx));
        drop(thread_rx);
        event!("Dropped On The Floor");
        init_for_thread(None);
    }
}
