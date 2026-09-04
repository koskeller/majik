//! The Telemetry Log (Zed's `telemetry_log.rs`): every usage event this session queued, newest
//! first, over what `telemetry.log` still holds from before, so the user can read exactly what
//! leaves the machine. Rendered inside the Settings window's Telemetry page.

use gpui::{prelude::*, px, App, Context, Entity, SharedString, Task, Window};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Sizable as _};
use majik_telemetry::{EventWrapper, FlexibleEvent};
use std::collections::{HashSet, VecDeque};

use crate::state;
use crate::ui::{button, icon};

/// How many events are kept; older ones fall off the end.
const MAX_EVENTS: usize = 10_000;
/// How many rows are laid out at once: the page scrolls as a whole, so the list is capped rather
/// than virtualised, and a line says how many more the filter matched.
const MAX_ROWS: usize = 200;

struct Entry {
    id: u64,
    /// When this process saw the event: for a live event, now; for one read back from the log,
    /// the moment the log was read (the log carries no wall-clock time, by design).
    received_at: chrono::DateTime<chrono::Local>,
    event: FlexibleEvent,
    /// `key=value …`, what the row shows collapsed.
    summary: SharedString,
    /// The properties as indented JSON, what the row shows expanded.
    detail: SharedString,
}

pub struct TelemetryLogView {
    /// Newest first.
    entries: VecDeque<Entry>,
    next_id: u64,
    expanded: HashSet<u64>,
    filter: Entity<InputState>,
    /// Lines of the log that were not events, so the page can say the file was tampered with.
    parse_errors: usize,
    read_error: Option<SharedString>,
    _feed: Task<()>,
}

impl TelemetryLogView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter events"));
        cx.subscribe(&filter, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();

        let telemetry = state::telemetry(cx);
        let feed = cx.spawn(async move |this, cx| {
            // The log first (blocking IO, so off the UI thread), then what is queued, then live.
            let history = cx.background_spawn({
                let telemetry = telemetry.clone();
                async move { telemetry.read_log() }
            })
            .await;
            let mut subscription = telemetry.subscribe();
            this.update(cx, |this, cx| {
                match history {
                    Ok(history) => {
                        this.parse_errors = history.parse_error_count;
                        this.push_events(history.events, cx);
                    }
                    Err(e) => this.read_error = Some(format!("Couldn't read the telemetry log: {e:#}").into()),
                }
                let queued = std::mem::take(&mut subscription.queued);
                this.push_events(queued, cx);
            })
            .ok();
            use futures::StreamExt as _;
            while let Some(event) = subscription.live.next().await {
                if this.update(cx, |this, cx| this.push_events([event], cx)).is_err() {
                    break;
                }
            }
        });

        Self { entries: VecDeque::new(), next_id: 0, expanded: HashSet::new(), filter, parse_errors: 0, read_error: None, _feed: feed }
    }

    fn push_events(&mut self, events: impl IntoIterator<Item = EventWrapper>, cx: &mut Context<Self>) {
        let now = chrono::Local::now();
        for wrapper in events {
            let event = wrapper.event;
            let summary = event.event_properties.iter().map(|(key, value)| format!("{key}={}", compact(value))).collect::<Vec<_>>().join("  ");
            let detail = serde_json::to_string_pretty(&event.event_properties).unwrap_or_default();
            self.entries.push_front(Entry { id: self.next_id, received_at: now, event, summary: summary.into(), detail: detail.into() });
            self.next_id += 1;
        }
        while self.entries.len() > MAX_EVENTS {
            if let Some(dropped) = self.entries.pop_back() {
                self.expanded.remove(&dropped.id);
            }
        }
        cx.notify();
    }

    /// Forget every event shown and empty the log file.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        self.expanded.clear();
        self.parse_errors = 0;
        if let Err(e) = state::telemetry(cx).clear_log() {
            self.read_error = Some(format!("Couldn't clear the telemetry log: {e:#}").into());
        }
        cx.notify();
    }

    fn toggle(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    /// The filter box's text, lower-cased; read at render time rather than kept in step with it.
    fn query(&self, cx: &App) -> String {
        self.filter.read(cx).value().trim().to_lowercase()
    }

    fn matches(entry: &Entry, query: &str) -> bool {
        query.is_empty() || entry.event.event_type.to_lowercase().contains(query) || entry.summary.to_lowercase().contains(query)
    }

    /// The event names shown, newest first, for tests.
    #[cfg(test)]
    pub fn shown(&self, cx: &App) -> Vec<String> {
        let query = self.query(cx);
        self.entries.iter().filter(|entry| Self::matches(entry, &query)).map(|entry| entry.event.event_type.clone()).collect()
    }

    #[cfg(test)]
    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        let query = query.to_string();
        self.filter.update(cx, |input, cx| input.set_value(query, window, cx));
    }
}

/// A property value on one line, short enough for a row.
fn compact(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.chars().count() > 40 {
        format!("{}…", text.chars().take(39).collect::<String>())
    } else {
        text
    }
}

impl Render for TelemetryLogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border, hover, danger) = (theme.muted_foreground, theme.border, theme.secondary, theme.danger);
        let query = self.query(cx);
        let shown: Vec<&Entry> = self.entries.iter().filter(|entry| Self::matches(entry, &query)).collect();
        let total = shown.len();
        let mut rows = v_flex().w_full();
        for entry in shown.into_iter().take(MAX_ROWS) {
            let id = entry.id;
            let expanded = self.expanded.contains(&id);
            let header = h_flex()
                .gap_2()
                .items_center()
                .child(icon(if expanded { "chevron-down" } else { "chevron-right" }).size_3().text_color(muted))
                .child(gpui::div().text_xs().text_color(muted).font_family("monospace").child(entry.received_at.format("%H:%M:%S").to_string()))
                .child(gpui::div().text_sm().child(entry.event.event_type.clone()))
                .when(!expanded && !entry.summary.is_empty(), |this| this.child(gpui::div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(entry.summary.clone())));
            rows = rows.child(
                v_flex()
                    .id(("telemetry-event", id))
                    .w_full()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(border)
                    .cursor_pointer()
                    .hover(|this| this.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(id, cx)))
                    .child(header)
                    .when(expanded, |this| this.child(gpui::div().pl_5().text_xs().font_family("monospace").whitespace_normal().child(entry.detail.clone()))),
            );
        }
        let clear = button("telemetry-log-clear").label("Clear").small().outline().on_click(cx.listener(|this, _, _, cx| this.clear(cx)));
        v_flex()
            .id("telemetry-log")
            .debug_selector(|| "telemetry-log".into())
            .w_full()
            .gap_2()
            .child(h_flex().gap_2().items_center().child(gpui::div().flex_1().child(Input::new(&self.filter).small())).child(clear))
            .when_some(self.read_error.clone(), |this, error| this.child(gpui::div().text_xs().text_color(danger).child(error)))
            .when(self.parse_errors > 0, |this| {
                this.child(gpui::div().text_xs().text_color(muted).child(format!("{} line(s) of the log could not be read as events.", self.parse_errors)))
            })
            .when(total == 0, |this| this.child(gpui::div().py_4().text_sm().text_color(muted).child(if query.is_empty() { "No events yet." } else { "No events match." })))
            .child(rows)
            .when(total > MAX_ROWS, |this| this.child(gpui::div().py_2().text_xs().text_color(muted).child(format!("{} older events not shown.", total - MAX_ROWS))))
            .child(gpui::div().h(px(0.)))
    }
}
