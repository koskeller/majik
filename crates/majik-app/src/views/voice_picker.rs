//! Voice picker, the model picker's palette over a model's voices: a search field on top and one
//! row per voice (a preview play button, the name, the provider's blurb, gender / accent /
//! language chips), with the current voice checked. Typing filters the rows, ↑/↓ move the
//! highlight, Enter picks and Escape closes. Speaker 2 gets a `None` row first, since a
//! dialogue can do without it. Previews are downloaded once into the OS cache dir.

use gpui::{prelude::*, px, App, Entity, ScrollStrategy, SharedString, Task, WeakEntity, Window};
use gpui_component::button::ButtonVariants as _;
use gpui_component::input::InputState;
use gpui_component::list::{ListDelegate, ListItem, ListState};
use gpui_component::{h_flex, ActiveTheme as _, IndexPath, Selectable, Sizable as _, WindowExt as _};
use majik_providers::AudioVoice;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::ui::{button, icon, pill};
use crate::views::compose::ComposeView;
use crate::views::model_picker::{open_palette, PaletteExtras};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    One,
    Two,
}

/// The playing side of a preview; `majik_audio::Player` in the app, a fake in tests.
pub trait PreviewPlayback {
    fn stop(&mut self);
    fn is_playing(&self) -> bool;
    fn finished(&self) -> bool;
}

impl PreviewPlayback for majik_audio::Player {
    fn stop(&mut self) {
        majik_audio::Player::stop(self);
    }

    fn is_playing(&self) -> bool {
        majik_audio::Player::is_playing(self)
    }

    fn finished(&self) -> bool {
        majik_audio::Player::finished(self)
    }
}

/// Downloads a preview by URL (on a background thread).
pub type FetchPreview = Arc<dyn Fn(&str) -> anyhow::Result<Vec<u8>> + Send + Sync>;
/// Opens the cached file and starts playing it (on the UI thread).
pub type OpenPreview = Rc<dyn Fn(&Path) -> anyhow::Result<Box<dyn PreviewPlayback>>>;

/// How previews are fetched and played. The defaults hit the network and the audio device;
/// tests substitute both so the state machine runs headlessly.
pub struct PreviewHooks {
    pub fetch: FetchPreview,
    pub open: OpenPreview,
    pub cache_dir: PathBuf,
}

impl Default for PreviewHooks {
    fn default() -> Self {
        Self {
            fetch: Arc::new(|url| {
                let response = reqwest::blocking::get(url)?;
                if !response.status().is_success() {
                    anyhow::bail!("HTTP {}", response.status());
                }
                Ok(response.bytes()?.to_vec())
            }),
            open: Rc::new(|path| {
                let mut player = majik_audio::Player::open(path)?;
                player.play();
                Ok(Box::new(player))
            }),
            // The channel's own cache, so wiping a dev install takes its downloads with it.
            cache_dir: crate::config::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("majik-voice-previews")).join("voice-previews"),
        }
    }
}

pub struct PreviewState {
    player: Option<(String, Box<dyn PreviewPlayback>)>,
    downloading: Option<String>,
    error: Option<SharedString>,
    /// A single repeating refresh while a preview plays (so the play/pause icon flips on finish).
    ticker: Option<Task<()>>,
    hooks: PreviewHooks,
}

impl PreviewState {
    pub fn new(hooks: PreviewHooks) -> Self {
        Self { player: None, downloading: None, error: None, ticker: None, hooks }
    }

    fn stop(&mut self) {
        if let Some((_, p)) = &mut self.player {
            p.stop();
        }
        self.player = None;
    }

    fn playing_id(&self) -> Option<&str> {
        self.player.as_ref().filter(|(_, p)| p.is_playing() && !p.finished()).map(|(id, _)| id.as_str())
    }

    fn cache_path(&self, voice: &AudioVoice, url: &str) -> PathBuf {
        let file = url.rsplit('/').next().unwrap_or("preview.mp3").split('?').next().unwrap_or("preview.mp3");
        let id: String = voice.id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        self.hooks.cache_dir.join(format!("{id}-{file}"))
    }

    /// Start the refresh ticker if a preview is playing and none runs. It notifies every 250 ms
    /// and stops (clearing itself) as soon as nothing is playing, so plays don't pile up timers.
    fn tick_while_playing(&mut self, cx: &mut Context<Self>) {
        if self.playing_id().is_none() || self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let keep = this.update(cx, |s, cx| {
                cx.notify();
                s.playing_id().is_some()
            });
            if !matches!(keep, Ok(true)) {
                this.update(cx, |s, _| s.ticker = None).ok();
                break;
            }
        }));
    }
}

/// Play `voice`'s preview, or stop it if it is the one playing. Any other preview stops first.
fn toggle_preview(state: &Entity<PreviewState>, voice: AudioVoice, cx: &mut App) {
    let Some(url) = voice.preview_url.clone() else { return };
    let already = state.read(cx).playing_id() == Some(voice.id.as_str());
    state.update(cx, |s, _| s.stop());
    if already {
        state.update(cx, |_, cx| cx.notify());
        return;
    }
    let path = state.read(cx).cache_path(&voice, &url);
    let id = voice.id.clone();
    state.update(cx, |s, cx| {
        s.downloading = Some(id.clone());
        s.error = None;
        cx.notify();
        let fetch = s.hooks.fetch.clone();
        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let fetched: anyhow::Result<PathBuf> = cx
                .background_spawn(async move {
                    if !p.exists() {
                        if let Some(dir) = p.parent() {
                            std::fs::create_dir_all(dir)?;
                        }
                        // Write beside the entry and rename in: two installs can download the same
                        // preview at once, and a half-written file would be played as a truncated clip.
                        let staged = p.with_extension(format!("{}.part", std::process::id()));
                        std::fs::write(&staged, fetch(&url)?)?;
                        std::fs::rename(&staged, &p)?;
                    }
                    Ok(p)
                })
                .await;
            this.update(cx, |s, cx| {
                s.downloading = None;
                match fetched.and_then(|p| (s.hooks.open)(&p)) {
                    Ok(player) => {
                        s.player = Some((id.clone(), player));
                        s.tick_while_playing(cx);
                    }
                    Err(e) => s.error = Some(format!("Preview failed: {e}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    });
}

/// One row of the picker: a voice, or the `None` row that clears Speaker 2.
#[derive(Clone, Debug)]
pub struct VoiceRow {
    pub voice: Option<AudioVoice>,
    pub name: String,
    pub subtitle: Option<String>,
    pub chips: Vec<String>,
}

fn capitalised(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl VoiceRow {
    pub fn none() -> Self {
        Self { voice: None, name: "None".into(), subtitle: None, chips: Vec::new() }
    }

    /// A voice's row: its blurb as the subtitle and its gender, accent and languages as chips.
    /// Language codes are shown upper-case (`EN`); the catalogs' `multilingual` is a word, not a code.
    pub fn from_voice(voice: &AudioVoice) -> Self {
        let mut chips: Vec<String> = [voice.gender.as_deref(), voice.accent.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).map(capitalised).collect();
        for code in voice.language_codes.iter().flatten().filter(|c| !c.is_empty()) {
            chips.push(if code.eq_ignore_ascii_case("multilingual") { "Multilingual".into() } else { code.to_uppercase() });
        }
        Self { voice: Some(voice.clone()), name: voice.display_name.clone(), subtitle: voice.subtitle.clone().filter(|s| !s.is_empty()), chips }
    }

    pub fn id(&self) -> Option<&str> {
        self.voice.as_ref().map(|v| v.id.as_str())
    }

    /// Case-insensitive match: every whitespace-separated term of `query` must occur in the name,
    /// the subtitle or a chip. An empty query matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {} {}", self.name, self.subtitle.as_deref().unwrap_or(""), self.chips.join(" ")).to_lowercase();
        query.split_whitespace().all(|term| haystack.contains(&term.to_lowercase()))
    }
}

/// Every row is one line at one height, because the list is virtualised and measures a single
/// row for all of them.
const ROW_HEIGHT: f32 = 40.;
/// The preview button's slot, the width of a small icon button; rows without one keep it empty
/// so the names line up.
const PREVIEW_SLOT: f32 = 24.;

pub struct VoicePickerDelegate {
    compose: WeakEntity<ComposeView>,
    speaker: Speaker,
    all: Vec<VoiceRow>,
    matched: Vec<VoiceRow>,
    /// The id of the voice the composer currently uses for this speaker; `None` marks the `None` row.
    current: Option<String>,
    selected: Option<IndexPath>,
    preview: Entity<PreviewState>,
}

impl VoicePickerDelegate {
    fn new(compose: WeakEntity<ComposeView>, speaker: Speaker, all: Vec<VoiceRow>, current: Option<String>, preview: Entity<PreviewState>) -> Self {
        Self { compose, speaker, matched: all.clone(), all, current, selected: None, preview }
    }

    /// Where the composer's current voice is listed, or the top when it is not offered.
    fn path_of_current(&self) -> IndexPath {
        IndexPath::new(self.matched.iter().position(|row| row.id() == self.current.as_deref()).unwrap_or(0))
    }

    /// Names of the rows currently shown, in order.
    #[cfg(test)]
    pub(crate) fn matched_names(&self) -> Vec<String> {
        self.matched.iter().map(|row| row.name.clone()).collect()
    }

    #[cfg(test)]
    pub(crate) fn preview_state(&self) -> &Entity<PreviewState> {
        &self.preview
    }

    /// The row at `ix` as the picker draws it.
    #[cfg(test)]
    pub(crate) fn row(&self, ix: usize) -> Option<&VoiceRow> {
        self.matched.get(ix)
    }
}

impl ListDelegate for VoicePickerDelegate {
    type Item = VoiceListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matched.len()
    }

    fn perform_search(&mut self, query: &str, _window: &mut Window, _cx: &mut Context<ListState<Self>>) -> Task<()> {
        self.matched = self.all.iter().filter(|row| row.matches(query)).cloned().collect();
        Task::ready(())
    }

    fn render_item(&mut self, ix: IndexPath, _window: &mut Window, cx: &mut Context<ListState<Self>>) -> Option<Self::Item> {
        let row = self.matched.get(ix.row)?.clone();
        let current = row.id() == self.current.as_deref();
        let preview = self.preview.read(cx);
        let playing = row.id().is_some() && preview.playing_id() == row.id();
        let downloading = row.id().is_some() && preview.downloading.as_deref() == row.id();
        Some(VoiceListItem { base: ListItem::new(ix), row, current, selected: false, playing, downloading, preview: self.preview.clone() })
    }

    fn render_empty(&mut self, _window: &mut Window, cx: &mut Context<ListState<Self>>) -> impl IntoElement {
        gpui::div().p_4().text_sm().text_color(cx.theme().muted_foreground).child("No voices match")
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _window: &mut Window, cx: &mut Context<ListState<Self>>) {
        self.selected = ix;
        cx.notify();
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(row) = self.selected.and_then(|ix| self.matched.get(ix.row)) else { return };
        let voice = row.voice.clone();
        let speaker = self.speaker;
        self.preview.update(cx, |s, _| s.stop());
        self.compose.update(cx, |view, cx| view.set_voice(speaker, voice, window, cx)).ok();
        window.close_dialog(cx);
    }
}

/// One voice row: the list highlights it (`selected`) as the keyboard moves; the composer's
/// current voice carries a check mark. The play button previews without picking.
#[derive(IntoElement)]
pub struct VoiceListItem {
    base: ListItem,
    row: VoiceRow,
    current: bool,
    selected: bool,
    playing: bool,
    downloading: bool,
    preview: Entity<PreviewState>,
}

impl Selectable for VoiceListItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self.base = self.base.selected(selected);
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for VoiceListItem {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (muted_fg, primary) = (theme.muted_foreground, theme.primary);
        let mut chips = h_flex().min_w_0().gap_1().flex_nowrap().overflow_hidden();
        for chip in &self.row.chips {
            chips = chips.child(pill(chip.clone(), cx));
        }
        let slot = gpui::div().flex_none().size(px(PREVIEW_SLOT)).flex().items_center().justify_center();
        let slot = match self.row.voice.clone().filter(|v| v.preview_url.is_some()) {
            Some(voice) => {
                let preview = self.preview.clone();
                slot.child(
                    button(SharedString::from(format!("preview-{}", voice.id)))
                        .icon(icon(if self.playing { "pause" } else { "play" }))
                        .loading(self.downloading)
                        .loading_icon(icon("loader-circle"))
                        .ghost()
                        .small()
                        .on_click(move |_, _, cx| {
                            // The row around the button is what picks; a preview only listens.
                            cx.stop_propagation();
                            toggle_preview(&preview, voice.clone(), cx);
                        }),
                )
            }
            None => slot,
        };
        self.base.h(px(ROW_HEIGHT)).px_2().py_0().rounded_md().overflow_hidden().child(
            h_flex()
                .w_full()
                .h_full()
                .gap_3()
                .items_center()
                .child(slot)
                .child(gpui::div().flex_none().font_weight(gpui::FontWeight::MEDIUM).whitespace_nowrap().child(self.row.name.clone()))
                .child(gpui::div().flex_1().min_w_0().text_sm().text_color(muted_fg).whitespace_nowrap().text_ellipsis().overflow_hidden().child(self.row.subtitle.clone().unwrap_or_default()))
                .when(!self.row.chips.is_empty(), |d| d.child(chips))
                .when(self.current, |d| d.child(icon("check").size_4().flex_none().text_color(primary))),
        )
    }
}

/// The picker's rows: the `None` row first when the speaker can do without a voice.
pub fn rows(voices: &[AudioVoice], allow_none: bool) -> Vec<VoiceRow> {
    allow_none.then(VoiceRow::none).into_iter().chain(voices.iter().map(VoiceRow::from_voice)).collect()
}

/// Opens the picker over the composer with the search field focused and the current voice
/// highlighted. Returns the list and the search field so tests can inspect them.
pub fn open_voice_picker(compose: WeakEntity<ComposeView>, speaker: Speaker, voices: Vec<AudioVoice>, current: Option<AudioVoice>, allow_none: bool, window: &mut Window, cx: &mut App) -> (Entity<ListState<VoicePickerDelegate>>, Entity<InputState>) {
    open_voice_picker_with_hooks(compose, speaker, voices, current, allow_none, PreviewHooks::default(), window, cx)
}

#[allow(clippy::too_many_arguments)]
pub fn open_voice_picker_with_hooks(compose: WeakEntity<ComposeView>, speaker: Speaker, voices: Vec<AudioVoice>, current: Option<AudioVoice>, allow_none: bool, hooks: PreviewHooks, window: &mut Window, cx: &mut App) -> (Entity<ListState<VoicePickerDelegate>>, Entity<InputState>) {
    let preview = cx.new(|_| PreviewState::new(hooks));
    let delegate = VoicePickerDelegate::new(compose, speaker, rows(&voices, allow_none), current.map(|v| v.id), preview.clone());
    let path = delegate.path_of_current();
    let list = cx.new(|cx| ListState::new(delegate, window, cx));
    list.update(cx, |list, cx| {
        list.set_selected_index(Some(path), window, cx);
        list.scroll_to_item(path, ScrollStrategy::Center, window, cx);
        // A preview starting, stopping or failing redraws its row's button.
        cx.observe(&preview, |_, _, cx| cx.notify()).detach();
    });
    let placeholder = match speaker {
        Speaker::One => "Search voices for Speaker 1",
        Speaker::Two => "Search voices for Speaker 2",
    };
    let (preview_for_banner, preview_for_dismiss) = (preview.clone(), preview);
    let extras = PaletteExtras {
        banner: Some(Rc::new(move |_, cx| {
            let error = preview_for_banner.read(cx).error.clone()?;
            Some(gpui::div().px_2().text_xs().text_color(cx.theme().danger).child(error).into_any_element())
        })),
        on_dismiss: Some(Rc::new(move |_, cx| preview_for_dismiss.update(cx, |s, _| s.stop()))),
    };
    let search = open_palette(list.clone(), placeholder, extras, window, cx);
    (list, search)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::compose_with_dialogs as compose_window;
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use majik_core::model::MediaType;
    use std::cell::{Cell, RefCell};

    /// A preview "player" that only remembers whether it was stopped.
    struct FakePlayback(Rc<Cell<bool>>);

    impl PreviewPlayback for FakePlayback {
        fn stop(&mut self) {
            self.0.set(false);
        }

        fn is_playing(&self) -> bool {
            self.0.get()
        }

        fn finished(&self) -> bool {
            false
        }
    }

    /// One flag per opened player, in open order; `false` once stopped.
    type Players = Rc<RefCell<Vec<Rc<Cell<bool>>>>>;
    type Fetches = Arc<std::sync::Mutex<Vec<String>>>;

    fn fake_hooks(dir: &Path, fetch_fails: bool) -> (PreviewHooks, Players, Fetches) {
        let players: Players = Default::default();
        let fetches: Fetches = Default::default();
        let (players2, fetches2) = (players.clone(), fetches.clone());
        let hooks = PreviewHooks {
            fetch: Arc::new(move |url| {
                fetches2.lock().unwrap().push(url.to_string());
                if fetch_fails {
                    anyhow::bail!("HTTP 404")
                }
                Ok(b"RIFF".to_vec())
            }),
            open: Rc::new(move |_| {
                let flag = Rc::new(Cell::new(true));
                players2.borrow_mut().push(flag.clone());
                Ok(Box::new(FakePlayback(flag)))
            }),
            cache_dir: dir.join("previews"),
        };
        (hooks, players, fetches)
    }

    struct Harness {
        state: Entity<PreviewState>,
        players: Players,
        fetches: Fetches,
        _dir: tempfile::TempDir,
    }

    fn harness(cx: &mut TestAppContext, fetch_fails: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let (hooks, players, fetches) = fake_hooks(dir.path(), fetch_fails);
        let state = cx.new(|_| PreviewState::new(hooks));
        Harness { state, players, fetches, _dir: dir }
    }

    fn voice(id: &str) -> AudioVoice {
        AudioVoice { preview_url: Some(format!("https://voices.example/{id}.mp3")), ..AudioVoice::new(id, id) }
    }

    fn toggle(h: &Harness, cx: &mut TestAppContext, id: &str) {
        let voice = voice(id);
        cx.update(|cx| toggle_preview(&h.state, voice, cx));
    }

    fn playing(h: &Harness, cx: &mut TestAppContext) -> Option<String> {
        h.state.read_with(cx, |s, _| s.playing_id().map(str::to_string))
    }

    fn downloading(h: &Harness, cx: &mut TestAppContext) -> Option<String> {
        h.state.read_with(cx, |s, _| s.downloading.clone())
    }

    // ----- the preview state machine -----------------------------------------------------------

    #[gpui::test]
    fn preview_shows_a_spinner_while_downloading_then_plays(cx: &mut TestAppContext) {
        let h = harness(cx, false);
        toggle(&h, cx, "Rachel");
        assert_eq!(downloading(&h, cx).as_deref(), Some("Rachel"), "spinner on the tapped row until the download lands");
        assert_eq!(playing(&h, cx), None);
        cx.run_until_parked();
        assert_eq!(downloading(&h, cx), None);
        assert_eq!(playing(&h, cx).as_deref(), Some("Rachel"));
        let cached = h.state.read_with(cx, |s, _| s.cache_path(&voice("Rachel"), "https://voices.example/Rachel.mp3"));
        assert_eq!(std::fs::read(cached).unwrap(), b"RIFF", "the preview is cached on disk");
    }

    #[gpui::test]
    fn switching_voices_stops_the_previous_preview(cx: &mut TestAppContext) {
        let h = harness(cx, false);
        toggle(&h, cx, "Rachel");
        cx.run_until_parked();
        toggle(&h, cx, "Adam");
        assert_eq!(playing(&h, cx), None, "the old preview stops the moment the new one is tapped");
        cx.run_until_parked();
        assert_eq!(playing(&h, cx).as_deref(), Some("Adam"));
        let flags: Vec<bool> = h.players.borrow().iter().map(|f| f.get()).collect();
        assert_eq!(flags, [false, true], "first player stopped, second playing");
    }

    #[gpui::test]
    fn tapping_the_playing_preview_stops_it(cx: &mut TestAppContext) {
        let h = harness(cx, false);
        toggle(&h, cx, "Rachel");
        cx.run_until_parked();
        toggle(&h, cx, "Rachel");
        cx.run_until_parked();
        assert_eq!(playing(&h, cx), None);
        assert_eq!(downloading(&h, cx), None, "a stop never starts a download");
        assert_eq!(h.players.borrow().len(), 1);
    }

    #[gpui::test]
    fn a_cached_preview_is_not_fetched_again(cx: &mut TestAppContext) {
        let h = harness(cx, false);
        toggle(&h, cx, "Rachel");
        cx.run_until_parked();
        toggle(&h, cx, "Rachel");
        toggle(&h, cx, "Rachel");
        cx.run_until_parked();
        assert_eq!(playing(&h, cx).as_deref(), Some("Rachel"));
        assert_eq!(h.fetches.lock().unwrap().len(), 1, "second play comes from the cache");
    }

    #[gpui::test]
    fn a_failed_download_shows_an_error_instead_of_playing(cx: &mut TestAppContext) {
        let h = harness(cx, true);
        toggle(&h, cx, "Rachel");
        cx.run_until_parked();
        assert_eq!(playing(&h, cx), None);
        assert_eq!(downloading(&h, cx), None);
        let error = h.state.read_with(cx, |s, _| s.error.clone());
        assert!(error.as_deref().unwrap_or("").starts_with("Preview failed"), "{error:?}");
        assert!(h.players.borrow().is_empty());
    }

    #[gpui::test]
    fn a_voice_without_a_preview_url_does_nothing(cx: &mut TestAppContext) {
        let h = harness(cx, false);
        cx.update(|cx| toggle_preview(&h.state, AudioVoice::new("Mute", "Mute"), cx));
        cx.run_until_parked();
        assert_eq!(downloading(&h, cx), None);
        assert!(h.fetches.lock().unwrap().is_empty());
    }

    // ----- rows ---------------------------------------------------------------------------------

    #[test]
    fn rows_carry_gender_accent_and_language_chips() {
        let eleven = AudioVoice { subtitle: Some("Clear, Calm, Natural".into()), gender: Some("female".into()), accent: Some("canadian".into()), language_codes: Some(vec!["en".into(), "de".into()]), ..AudioVoice::new("Rachel", "Rachel") };
        let row = VoiceRow::from_voice(&eleven);
        assert_eq!(row.chips, ["Female", "Canadian", "EN", "DE"]);
        assert_eq!(row.subtitle.as_deref(), Some("Clear, Calm, Natural"));

        let gemini = AudioVoice { gender: Some("male".into()), language_codes: Some(vec!["multilingual".into()]), ..AudioVoice::new("Puck", "Puck") };
        assert_eq!(VoiceRow::from_voice(&gemini).chips, ["Male", "Multilingual"], "the catalog's word is shown as a word, not a code");

        let bare = AudioVoice { gender: Some(String::new()), subtitle: Some(String::new()), ..AudioVoice::new("Old", "Old") };
        let row = VoiceRow::from_voice(&bare);
        assert!(row.chips.is_empty(), "empty fields make no chips: {:?}", row.chips);
        assert_eq!(row.subtitle, None, "an empty blurb is no blurb");
    }

    #[test]
    fn matches_is_case_insensitive_over_name_subtitle_and_chips() {
        let voice = AudioVoice { subtitle: Some("Velvety narrator".into()), gender: Some("female".into()), accent: Some("british".into()), language_codes: Some(vec!["en".into()]), ..AudioVoice::new("Alice", "Alice") };
        let row = VoiceRow::from_voice(&voice);
        assert!(row.matches(""));
        assert!(row.matches("ALICE"));
        assert!(row.matches("velvety"));
        assert!(row.matches("british"));
        assert!(row.matches("female en"), "every term has to match, in any order");
        assert!(!row.matches("male scottish"));
        assert!(!row.matches("adam"));
        let none = VoiceRow::none();
        assert!(none.matches("") && none.matches("no"));
        assert!(!none.matches("alice"), "the None row is not a match for a voice name");
    }

    #[test]
    fn the_none_row_leads_only_when_allowed() {
        let voices = [voice("Rachel"), voice("Adam")];
        let names = |allow_none| rows(&voices, allow_none).iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(true), ["None", "Rachel", "Adam"]);
        assert_eq!(names(false), ["Rachel", "Adam"]);
    }

    // ----- the palette over the composer ---------------------------------------------------------

    type Picker = (Entity<ListState<VoicePickerDelegate>>, Entity<InputState>);

    fn audio_composer(cx: &mut TestAppContext) -> (Entity<ComposeView>, &mut VisualTestContext, Vec<AudioVoice>) {
        let (view, vcx) = compose_window(cx);
        view.update_in(vcx, |v, window, cx| v.set_media_type(MediaType::Audio, window, cx));
        vcx.run_until_parked();
        let voices = view.read_with(vcx, |v, _| v.composer_state().audio_caps().expect("the Mock audio model has voices").supported_voices.clone());
        assert!(voices.len() > 3, "the suite needs a few voices to move between");
        (view, vcx, voices)
    }

    /// Opens the picker for `speaker` the way its capsule does, with fake preview hooks.
    fn open(view: &Entity<ComposeView>, vcx: &mut VisualTestContext, speaker: Speaker, dir: &Path) -> Picker {
        let (hooks, _, _) = fake_hooks(dir, false);
        let opened = view.update_in(vcx, |view, window, cx| {
            let voices = view.composer_state().audio_caps().unwrap().supported_voices.clone();
            let (current, allow_none) = match speaker {
                Speaker::One => (view.composer_state().audio.speaker1.clone(), false),
                Speaker::Two => (view.composer_state().audio.speaker2.clone(), true),
            };
            open_voice_picker_with_hooks(cx.entity().downgrade(), speaker, voices, current, allow_none, hooks, window, cx)
        });
        vcx.run_until_parked();
        opened
    }

    fn speaker_id(view: &Entity<ComposeView>, vcx: &mut VisualTestContext, speaker: Speaker) -> Option<String> {
        view.read_with(vcx, |v, _| match speaker {
            Speaker::One => v.composer_state().audio.speaker1.as_ref().map(|s| s.id.clone()),
            Speaker::Two => v.composer_state().audio.speaker2.as_ref().map(|s| s.id.clone()),
        })
    }

    fn shown(list: &Entity<ListState<VoicePickerDelegate>>, vcx: &mut VisualTestContext) -> Vec<String> {
        list.read_with(vcx, |list, _| list.delegate().matched_names())
    }

    fn highlighted(list: &Entity<ListState<VoicePickerDelegate>>, vcx: &mut VisualTestContext) -> Option<usize> {
        list.read_with(vcx, |list, _| list.selected_index().map(|ix| ix.row))
    }

    fn dialog_open(vcx: &mut VisualTestContext) -> bool {
        vcx.update(|window, cx| window.has_active_dialog(cx))
    }

    #[gpui::test]
    fn opens_with_the_search_focused_and_the_current_voice_highlighted(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let third = voices[2].clone();
        view.update_in(vcx, |v, window, cx| v.set_voice(Speaker::One, Some(third.clone()), window, cx));
        let (list, search) = open(&view, vcx, Speaker::One, dir.path());
        assert!(dialog_open(vcx), "the picker is a dialog over the composer");
        assert!(vcx.update(|window, cx| search.focus_handle(cx).is_focused(window)), "typing goes straight into the search field");
        assert_eq!(highlighted(&list, vcx), Some(2));
        assert_eq!(shown(&list, vcx).len(), voices.len(), "every voice of the model is listed before searching");
    }

    #[gpui::test]
    fn rows_show_the_catalog_metadata_as_chips(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        let rows: Vec<VoiceRow> = list.read_with(vcx, |list, _| (0..voices.len()).filter_map(|ix| list.delegate().row(ix).cloned()).collect());
        assert_eq!(rows.len(), voices.len());
        assert!(rows.iter().all(|r| !r.chips.is_empty()), "every Mock voice has metadata to show: {rows:?}");
        assert!(rows.iter().any(|r| r.subtitle.is_some()), "ElevenLabs blurbs reach the rows");
    }

    #[gpui::test]
    fn typing_filters_by_name_and_by_chip_and_highlights_the_first_match(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let last = voices.last().unwrap().clone();
        view.update_in(vcx, |v, window, cx| v.set_voice(Speaker::One, Some(last.clone()), window, cx));
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        assert_eq!(highlighted(&list, vcx), Some(voices.len() - 1));

        vcx.simulate_input("british");
        vcx.run_until_parked();
        let names = shown(&list, vcx);
        assert!(!names.is_empty() && names.len() < voices.len(), "{names:?}");
        assert!(voices.iter().filter(|v| names.contains(&v.display_name)).all(|v| v.accent.as_deref() == Some("british")), "{names:?}");
        assert_eq!(highlighted(&list, vcx), Some(0), "the highlight moves to the first match");
        assert!(dialog_open(vcx));

        vcx.simulate_keystrokes("secondary-a backspace");
        vcx.run_until_parked();
        assert_eq!(shown(&list, vcx).len(), voices.len(), "clearing the search brings every voice back");

        let name = voices[1].display_name.clone();
        vcx.simulate_input(&name.to_lowercase());
        vcx.run_until_parked();
        assert!(shown(&list, vcx).contains(&name), "search is case-insensitive over the name");
    }

    #[gpui::test]
    fn arrow_keys_move_the_highlight_and_enter_picks(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let first = voices[0].clone();
        view.update_in(vcx, |v, window, cx| v.set_voice(Speaker::One, Some(first.clone()), window, cx));
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        vcx.simulate_keystrokes("down down");
        assert_eq!(highlighted(&list, vcx), Some(2));
        vcx.simulate_keystrokes("up");
        assert_eq!(highlighted(&list, vcx), Some(1));
        assert_eq!(speaker_id(&view, vcx, Speaker::One).as_deref(), Some(voices[0].id.as_str()), "moving the highlight does not pick yet");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx), "enter closes the picker");
        assert_eq!(speaker_id(&view, vcx, Speaker::One).as_deref(), Some(voices[1].id.as_str()));
    }

    #[gpui::test]
    fn enter_picks_the_highlighted_match_not_its_row_number(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        let target = voices[voices.len() - 2].clone();
        vcx.simulate_input(&target.display_name);
        vcx.run_until_parked();
        assert_eq!(shown(&list, vcx).first(), Some(&target.display_name), "{:?}", shown(&list, vcx));
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(speaker_id(&view, vcx, Speaker::One), Some(target.id));
    }

    #[gpui::test]
    fn escape_and_a_click_outside_close_without_changing_the_voice(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let before = speaker_id(&view, vcx, Speaker::One);
        assert!(before.is_some());
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        vcx.simulate_keystrokes("down");
        assert_ne!(highlighted(&list, vcx), Some(0));
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx), "escape closes the picker");
        assert_eq!(speaker_id(&view, vcx, Speaker::One), before);

        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        assert_eq!(shown(&list, vcx).len(), voices.len());
        vcx.simulate_click(gpui::point(px(4.), px(200.)), gpui::Modifiers::default());
        vcx.run_until_parked();
        assert!(!dialog_open(vcx), "the picker has no close button; a click on the backdrop dismisses it");
        assert_eq!(speaker_id(&view, vcx, Speaker::One), before);
    }

    #[gpui::test]
    fn enter_with_no_match_keeps_the_picker_open(cx: &mut TestAppContext) {
        let (view, vcx, _) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let before = speaker_id(&view, vcx, Speaker::One);
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        vcx.simulate_input("no such voice");
        vcx.run_until_parked();
        assert!(shown(&list, vcx).is_empty(), "the empty state is shown");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(dialog_open(vcx), "nothing to pick, nothing to close");
        assert_eq!(speaker_id(&view, vcx, Speaker::One), before);
    }

    #[gpui::test]
    fn speaker_two_lists_a_none_row_first_and_picking_it_clears_the_voice(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        assert_ne!(shown(&list, vcx).first().map(String::as_str), Some("None"), "speaker 1 always has a voice");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();

        let (list, _) = open(&view, vcx, Speaker::Two, dir.path());
        assert_eq!(shown(&list, vcx).first().map(String::as_str), Some("None"));
        assert_eq!(highlighted(&list, vcx), Some(0), "speaker 2 starts on None, so None is highlighted");
        vcx.simulate_keystrokes("down enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(speaker_id(&view, vcx, Speaker::Two).as_deref(), Some(voices[0].id.as_str()));

        let (list, _) = open(&view, vcx, Speaker::Two, dir.path());
        assert_eq!(highlighted(&list, vcx), Some(1), "the picked voice, below the None row");
        vcx.simulate_input("none");
        vcx.run_until_parked();
        assert_eq!(shown(&list, vcx), ["None"], "the None row is searchable by its name");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(speaker_id(&view, vcx, Speaker::Two), None, "picking None clears speaker 2");
    }

    #[gpui::test]
    fn previewing_a_row_plays_it_without_picking_it(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let before = speaker_id(&view, vcx, Speaker::One);
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        let preview = list.read_with(vcx, |list, _| list.delegate().preview_state().clone());
        let sample = voices.iter().find(|v| v.preview_url.is_some()).expect("a voice with a preview").clone();
        vcx.update(|_, cx| toggle_preview(&preview, sample.clone(), cx));
        vcx.run_until_parked();
        assert_eq!(preview.read_with(vcx, |s, _| s.playing_id().map(str::to_string)), Some(sample.id.clone()));
        assert!(dialog_open(vcx), "a preview is not a pick");
        assert_eq!(speaker_id(&view, vcx, Speaker::One), before);

        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(preview.read_with(vcx, |s, _| s.playing_id().map(str::to_string)), None, "dismissing the picker stops the preview");
    }

    #[gpui::test]
    fn picking_a_voice_stops_the_preview(cx: &mut TestAppContext) {
        let (view, vcx, voices) = audio_composer(cx);
        let dir = tempfile::tempdir().unwrap();
        let (list, _) = open(&view, vcx, Speaker::One, dir.path());
        let preview = list.read_with(vcx, |list, _| list.delegate().preview_state().clone());
        let sample = voices.iter().find(|v| v.preview_url.is_some()).expect("a voice with a preview").clone();
        vcx.update(|_, cx| toggle_preview(&preview, sample.clone(), cx));
        vcx.run_until_parked();
        vcx.simulate_keystrokes("down enter");
        vcx.run_until_parked();
        assert!(!dialog_open(vcx));
        assert_eq!(preview.read_with(vcx, |s, _| s.playing_id().map(str::to_string)), None);
    }
}
