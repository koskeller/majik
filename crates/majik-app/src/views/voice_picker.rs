//! Voice picker sheet: voice rows with subtitle / metadata and a
//! preview play button; previews are downloaded once into the OS cache dir.

use gpui::{prelude::*, px, App, Entity, SharedString, WeakEntity, Window};
use gpui_component::button::{ButtonVariants as _};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Sizable as _, WindowExt as _};
use majik_providers::AudioVoice;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use crate::ui::{button, icon};
use crate::views::compose::ComposeView;

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
    ticker: Option<gpui::Task<()>>,
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
}

/// The metadata line under a voice's name: `gender · accent · LANG, LANG`, or nothing.
fn voice_meta(voice: &AudioVoice) -> Option<String> {
    let languages = voice.language_codes.as_ref().map(|l| l.iter().map(|c| c.to_uppercase()).collect::<Vec<_>>().join(", "));
    let parts: Vec<String> = [voice.gender.clone(), voice.accent.clone(), languages].into_iter().flatten().filter(|s| !s.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
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
                    Ok(player) => s.player = Some((id.clone(), player)),
                    Err(e) => s.error = Some(format!("Preview failed: {e}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    });
}

pub fn open_voice_picker(compose: WeakEntity<ComposeView>, speaker: Speaker, voices: Vec<AudioVoice>, current: Option<AudioVoice>, allow_none: bool, window: &mut Window, cx: &mut App) {
    let state = cx.new(|_| PreviewState::new(PreviewHooks::default()));
    let title = match speaker {
        Speaker::One => "Speaker 1",
        Speaker::Two => "Speaker 2",
    };
    let state_close = state.clone();
    window.open_dialog(cx, move |dialog, _window, cx| {
        let state = state.clone();
        let compose = compose.clone();
        let voices = voices.clone();
        let current = current.clone();
        dialog.title(title).w(px(560.)).child(render(&state, compose, speaker, voices, current, allow_none, cx)).on_close({
            let state = state_close.clone();
            move |_, _, cx| state.update(cx, |s, _| s.stop())
        })
    });
}

fn render(state: &Entity<PreviewState>, compose: WeakEntity<ComposeView>, speaker: Speaker, voices: Vec<AudioVoice>, current: Option<AudioVoice>, allow_none: bool, cx: &mut App) -> impl IntoElement {
    // A single self-cancelling ticker refreshes the icon while a preview plays; it stops (and clears
    // itself) as soon as nothing is playing, so re-renders don't spawn a growing pile of timers.
    state.update(cx, |s, cx| {
        if s.playing_id().is_some() && s.ticker.is_none() {
            s.ticker = Some(cx.spawn(async move |this, cx| loop {
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
    });
    let theme = cx.theme();
    let (muted, muted_fg, primary, border) = (theme.muted, theme.muted_foreground, theme.primary, theme.border);
    let playing = state.read(cx).playing_id().map(str::to_string);
    let downloading = state.read(cx).downloading.clone();
    let error = state.read(cx).error.clone();

    let mut list = v_flex().gap_1();
    if allow_none {
        let compose = compose.clone();
        let selected = current.is_none();
        list = list.child(
            h_flex()
                .id("voice-none")
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(if selected { primary } else { border })
                .cursor_pointer()
                .hover(move |s| s.bg(muted))
                .on_click(move |_, window, cx| {
                    compose.update(cx, |v, cx| v.set_voice(speaker, None, window, cx)).ok();
                    window.close_dialog(cx);
                })
                .child(gpui::div().flex_1().child("None"))
                .when(selected, |d| d.child(icon("check").size_4().text_color(primary))),
        );
    }
    for voice in voices {
        let selected = current.as_ref().map(|c| c.id == voice.id).unwrap_or(false);
        let is_playing = playing.as_deref() == Some(voice.id.as_str());
        let is_downloading = downloading.as_deref() == Some(voice.id.as_str());
        let meta = voice_meta(&voice);
        let compose = compose.clone();
        let pick_voice = voice.clone();
        let preview_voice = voice.clone();
        let st = state.clone();
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("voice-{}", voice.id)))
                .gap_2()
                .items_center()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(if selected { primary } else { border })
                .cursor_pointer()
                .hover(move |s| s.bg(muted))
                .on_click(move |_, window, cx| {
                    compose.update(cx, |v, cx| v.set_voice(speaker, Some(pick_voice.clone()), window, cx)).ok();
                    window.close_dialog(cx);
                })
                .when(voice.preview_url.is_some(), |d| {
                    d.child(
                        button(SharedString::from(format!("preview-{}", voice.id)))
                            .icon(icon(if is_playing { "pause" } else { "play" }))
                            .loading(is_downloading)
                            .loading_icon(icon("loader-circle"))
                            .ghost()
                            .small()
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                toggle_preview(&st, preview_voice.clone(), cx);
                            }),
                    )
                })
                .child(
                    v_flex()
                        .flex_1()
                        .child(gpui::div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child(voice.display_name.clone()))
                        .when_some(voice.subtitle.clone().filter(|s| !s.is_empty()), |d, s| d.child(gpui::div().text_xs().text_color(muted_fg).child(s)))
                        .when_some(meta, |d, meta| d.child(gpui::div().text_xs().text_color(muted_fg).child(meta))),
                )
                .when(selected, |d| d.child(icon("check").size_4().text_color(primary))),
        );
    }

    v_flex()
        .gap_2()
        .when_some(error, |d, e| d.child(gpui::div().text_xs().text_color(theme.danger).child(e)))
        .child(gpui::div().id("voice-list").max_h(px(520.)).overflow_y_scrollbar().child(list))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
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

    struct Harness {
        state: Entity<PreviewState>,
        /// One flag per opened player, in open order; `false` once stopped.
        players: Rc<RefCell<Vec<Rc<Cell<bool>>>>>,
        fetches: Arc<std::sync::Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    fn harness(cx: &mut TestAppContext, fetch_fails: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let players: Rc<RefCell<Vec<Rc<Cell<bool>>>>> = Default::default();
        let fetches: Arc<std::sync::Mutex<Vec<String>>> = Default::default();
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
            cache_dir: dir.path().join("previews"),
        };
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

    #[test]
    fn voice_meta_joins_gender_accent_and_languages() {
        let full = AudioVoice { gender: Some("female".into()), accent: Some("American".into()), language_codes: Some(vec!["en".into(), "de".into()]), ..AudioVoice::new("Rachel", "Rachel") };
        assert_eq!(voice_meta(&full).as_deref(), Some("female · American · EN, DE"));
        let partial = AudioVoice { gender: Some(String::new()), accent: Some("British".into()), ..AudioVoice::new("Adam", "Adam") };
        assert_eq!(voice_meta(&partial).as_deref(), Some("British"), "empty fields are skipped");
        assert_eq!(voice_meta(&AudioVoice::new("Kore", "Kore")), None, "no metadata, no row");
    }
}
