//! Shared HTTP plumbing: one `reqwest::Client` per timeout profile, JSON helpers, download, and
//! the tracing every exchange goes through.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use majik_core::model::{bound_body, JobTrace, TraceLabel};

use crate::client::TraceSink;
use crate::error::{GenerationError, Result};

/// The request timeout applies per request; `total` caps the whole operation (used by callers as a
/// deadline for polling loops).
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub request: Duration,
    pub total: Duration,
}

impl Timeouts {
    pub const IMAGE: Timeouts = Timeouts { request: Duration::from_secs(120), total: Duration::from_secs(360) };
    /// The floor for a video poll, and the budget for one whose length we don't know (a resume,
    /// where the engine's own remaining-time bound is the authoritative one). Prefer
    /// [`Timeouts::video`] wherever the requested duration is in hand.
    pub const VIDEO: Timeouts = Timeouts { request: Duration::from_secs(120), total: Duration::from_secs(600) };
    pub const AUDIO: Timeouts = Timeouts { request: Duration::from_secs(120), total: Duration::from_secs(240) };
    /// Rewriting a prompt is a short interactive call: a slow one is a failure, not a long job.
    pub const TEXT: Timeouts = Timeouts { request: Duration::from_secs(30), total: Duration::from_secs(30) };

    /// Queue overhead a render pays regardless of how long the clip is.
    const VIDEO_OVERHEAD: Duration = Duration::from_secs(300);
    /// Wall-clock we allow per second of output. Providers render at roughly a constant rate, so
    /// the budget has to grow with the clip: models now go to 30 s (Seedance 2.5, WAN 3.0) where
    /// the catalog previously stopped at 20 s, and a flat cap silently discarded renders the user
    /// had already paid for.
    const VIDEO_PER_OUTPUT_SECOND: Duration = Duration::from_secs(30);

    /// The longest clip any model in the catalog renders. `video_budget_covers_every_model` in
    /// `tests/shared.rs` fails if a model is ever added that asks for more.
    pub const MAX_VIDEO_OUTPUT_SECONDS: u32 = 30;

    /// The poll budget for a video of `duration_secs` of output. Never below [`Timeouts::VIDEO`],
    /// so no length that works today gets a shorter deadline than it has now.
    pub fn video(duration_secs: u32) -> Timeouts {
        let scaled = Self::VIDEO_OVERHEAD.saturating_add(Self::VIDEO_PER_OUTPUT_SECOND.saturating_mul(duration_secs));
        Timeouts { request: Self::VIDEO.request, total: Self::VIDEO.total.max(scaled) }
    }

    /// Re-attaching to a job left in flight by a previous run, which doesn't tell us how long the
    /// clip is. Polls on the budget of the longest clip we render and lets the engine's own
    /// remaining-time bound do the cutting — the alternative, [`Timeouts::VIDEO`], would abandon a
    /// resumed 30 s render at 10 minutes while the engine still considered it live.
    pub fn video_resume() -> Timeouts {
        Self::video(Self::MAX_VIDEO_OUTPUT_SECONDS)
    }
}

pub fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!("majik/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client")
    })
}

/// Send one request and report it to `sink`: method, URL, status, duration and both bodies
/// (bounded, data URIs elided, never the headers — the API key lives there). A transport error
/// is reported with no status and returned as it was. The `(status, bytes)` shape is what every
/// client already reduced its responses to.
pub async fn send_traced(request: reqwest::RequestBuilder, label: TraceLabel, sink: Option<&TraceSink>) -> std::result::Result<(u16, Vec<u8>), reqwest::Error> {
    let request = request.build()?;
    let method = request.method().to_string();
    let url = request.url().to_string();
    let request_body = sink.and_then(|_| request.body().and_then(|body| body.as_bytes()).and_then(trace_body));
    let started = Instant::now();
    let result = async {
        let response = client().execute(request).await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?.to_vec();
        Ok::<_, reqwest::Error>((status, bytes))
    }
    .await;
    if let Some(sink) = sink {
        let (status, response_body, error) = match &result {
            Ok((status, bytes)) => (Some(*status), trace_body(bytes), None),
            Err(e) => (None, None, Some(e.to_string())),
        };
        sink(JobTrace { at_ms: majik_core::now_ms(), label, method, url, status, duration_ms: started.elapsed().as_millis() as u64, request_body, response_body, error });
    }
    result
}

/// A body as the trail keeps it: JSON with its data URIs elided (a reference image inline would
/// swamp the entry), any other text as it is, both bounded; binary is left out.
fn trace_body(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        elide_data_uris(&mut value);
        return Some(bound_body(value.to_string()));
    }
    std::str::from_utf8(bytes).ok().map(|text| bound_body(text.to_string()))
}

fn elide_data_uris(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) if s.starts_with("data:") => {
            let head: String = s.chars().take_while(|c| *c != ',').collect();
            *s = format!("{head},…[{} bytes elided]", s.len());
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(elide_data_uris),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(elide_data_uris),
        _ => {}
    }
}

/// [`download`], reported to `sink` as a `Download` exchange: the file's size stands in for the
/// body.
pub async fn download_traced(url: &str, timeout: Duration, sink: Option<&TraceSink>) -> Result<Vec<u8>> {
    let started = Instant::now();
    let result = download(url, timeout).await;
    if let Some(sink) = sink {
        let (status, response_body, error) = match &result {
            Ok(bytes) => (Some(200), Some(format!("{} bytes", bytes.len())), None),
            Err(GenerationError::ServerError { status_code, message }) => (*status_code, None, Some(message.clone())),
            Err(e) => (None, None, Some(e.to_string())),
        };
        sink(JobTrace {
            at_ms: majik_core::now_ms(),
            label: TraceLabel::Download,
            method: "GET".into(),
            url: url.to_string(),
            status,
            duration_ms: started.elapsed().as_millis() as u64,
            request_body: None,
            response_body,
            error,
        });
    }
    result
}

/// Download a URL's body (result files from providers). A GET is idempotent, so one transport
/// failure (a pooled connection the server closed under us, a reset) is retried once on a fresh
/// connection rather than failing the whole generation; HTTP error statuses and timeouts are not.
pub async fn download(url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let resp = match client().get(url).timeout(timeout).send().await {
        Ok(resp) => resp,
        Err(e) if e.is_request() || e.is_connect() || e.is_body() => {
            tracing::debug!(target: "majik", "retrying download of {url} after a transport error: {e}");
            client().get(url).timeout(timeout).send().await?
        }
        Err(e) => return Err(e.into()),
    };
    let status = resp.status();
    if !status.is_success() {
        return Err(GenerationError::server(Some(status.as_u16()), format!("download failed for {url}")));
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Sleep helper so polling loops don't depend on a specific runtime API.
pub async fn sleep(d: Duration) {
    tokio::time::sleep(d).await
}

/// Backoff ladder shared by fal and Replicate polling: 3 s < 30 s elapsed, 5 s < 120 s, else 10 s.
pub fn poll_interval(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(30) {
        Duration::from_secs(3)
    } else if elapsed < Duration::from_secs(120) {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_core::model::TRACE_BODY_LIMIT;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    fn recording_sink() -> (TraceSink, Arc<Mutex<Vec<JobTrace>>>) {
        let seen: Arc<Mutex<Vec<JobTrace>>> = Default::default();
        let sink = seen.clone();
        (Arc::new(move |trace| sink.lock().unwrap().push(trace)), seen)
    }

    /// A server answering the first request with `status` and a JSON `body`.
    async fn json_server(status: &'static str, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/run", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await;
            let head = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        url
    }

    #[test]
    fn trace_body_elides_data_uris_bounds_text_and_skips_binary() {
        let json = br#"{"prompt":"a","image_url":"data:image/png;base64,AAAA","nested":[{"url":"data:x,yy"}]}"#;
        let out = trace_body(json).unwrap();
        assert!(out.contains("\"prompt\":\"a\""), "{out}");
        assert!(out.contains("data:image/png;base64,…[") && !out.contains("AAAA"), "{out}");
        assert!(out.contains("data:x,…[") && !out.contains(",yy"), "{out}");
        assert_eq!(trace_body(b"plain text").as_deref(), Some("plain text"));
        assert_eq!(trace_body(b""), None);
        assert_eq!(trace_body(&[0xff, 0xfe, 0x00]), None, "binary is left out");
        let big = format!("{{\"k\":\"{}\"}}", "x".repeat(TRACE_BODY_LIMIT * 2));
        assert!(trace_body(big.as_bytes()).unwrap().len() <= TRACE_BODY_LIMIT);
    }

    #[tokio::test]
    async fn send_traced_records_the_exchange_without_its_headers() {
        let url = json_server("202 Accepted", br#"{"request_id":"r1"}"#).await;
        let (sink, seen) = recording_sink();
        let request = client().post(&url).header("Authorization", "Key very-secret").body(r#"{"prompt":"p"}"#);
        let (status, bytes) = send_traced(request, TraceLabel::Submit, Some(&sink)).await.unwrap();
        assert_eq!((status, bytes.as_slice()), (202, br#"{"request_id":"r1"}"#.as_slice()));
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.len(), 1);
        let trace = &traces[0];
        assert_eq!((trace.label, trace.method.as_str(), trace.status), (TraceLabel::Submit, "POST", Some(202)));
        assert_eq!(trace.url, url);
        assert_eq!(trace.request_body.as_deref(), Some(r#"{"prompt":"p"}"#));
        assert_eq!(trace.response_body.as_deref(), Some(r#"{"request_id":"r1"}"#));
        assert!(trace.error.is_none());
        assert!(!format!("{trace:?}").contains("very-secret"), "headers are never recorded");
    }

    #[tokio::test]
    async fn send_traced_without_a_sink_just_sends() {
        let url = json_server("200 OK", b"{}").await;
        assert_eq!(send_traced(client().get(&url), TraceLabel::Poll, None).await.unwrap(), (200, b"{}".to_vec()));
    }

    #[tokio::test]
    async fn send_traced_records_a_transport_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/gone", listener.local_addr().unwrap());
        drop(listener);
        let (sink, seen) = recording_sink();
        assert!(send_traced(client().get(&url), TraceLabel::Poll, Some(&sink)).await.is_err());
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.len(), 1);
        assert_eq!((traces[0].label, traces[0].status), (TraceLabel::Poll, None));
        assert!(traces[0].error.as_deref().unwrap_or_default().contains("error sending request"), "{:?}", traces[0].error);
    }

    #[tokio::test]
    async fn download_traced_records_the_size_not_the_bytes() {
        let url = flaky_server(b"png-bytes").await;
        let (sink, seen) = recording_sink();
        assert_eq!(download_traced(&url, Duration::from_secs(5), Some(&sink)).await.unwrap(), b"png-bytes");
        let traces = seen.lock().unwrap().clone();
        assert_eq!(traces.len(), 1);
        assert_eq!((traces[0].label, traces[0].method.as_str(), traces[0].status), (TraceLabel::Download, "GET", Some(200)));
        assert_eq!((traces[0].request_body.as_deref(), traces[0].response_body.as_deref()), (None, Some("9 bytes")));
    }

    /// A server that drops the first connection unread and answers the second with `body`.
    async fn flaky_server(body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            drop(first);
            let (mut second, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = second.read(&mut request).await;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            second.write_all(head.as_bytes()).await.unwrap();
            second.write_all(body).await.unwrap();
            second.shutdown().await.unwrap();
        });
        url
    }

    #[tokio::test]
    async fn download_retries_once_after_a_dropped_connection() {
        let url = flaky_server(b"png-bytes").await;
        let bytes = download(&url, Duration::from_secs(5)).await.unwrap();
        assert_eq!(bytes, b"png-bytes");
    }

    #[tokio::test]
    async fn download_reports_an_http_error_status_without_retrying() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/missing", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut served = 0;
            while let Ok((mut stream, _)) = listener.accept().await {
                served += 1;
                assert_eq!(served, 1, "a 404 must not be retried");
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let error = download(&url, Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(error, GenerationError::ServerError { status_code: Some(404), .. }), "{error:?}");
    }

    #[tokio::test]
    async fn download_error_names_the_underlying_cause() {
        // Bind then drop, so the port is closed and the connect is refused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/gone", listener.local_addr().unwrap());
        drop(listener);
        let error = download(&url, Duration::from_secs(5)).await.unwrap_err();
        let GenerationError::Unknown(message) = &error else { panic!("{error:?}") };
        assert!(message.contains("error sending request") && message.matches(": ").count() >= 1, "source chain kept: {message}");
        assert!(message.to_ascii_lowercase().contains("refused") || message.to_ascii_lowercase().contains("connect"), "{message}");
    }
}
