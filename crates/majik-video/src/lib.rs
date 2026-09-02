//! Video playback for Majik: one software path on every platform.
//!
//! [`Player`] is the UI-thread state machine (play/pause/seek/loop, a clock, the frame on screen)
//! over a [`Source`] (`majik_core::video`: MP4 demux + H.264 decode) that lives behind a mutex so
//! decoding can run on a background thread. The crate owns no threads or timers: the app asks for a
//! [`DecodeJob`], runs it on its executor, and hands the [`DecodeResult`] back with
//! [`Player::apply`]. That keeps playback deterministic under GPUI's test executor, whose clock is
//! injected as [`Now`].
//!
//! When the file has an audio track, [`majik_audio::Player`] plays it and *is* the clock, so
//! frames follow the audio position. It is opened on the first [`Player::play`], not with the
//! player: opening an output device is synchronous UI-thread work (the sink is `!Send`), and a
//! video merely paged past never plays. Until then, for silent files, and when no output device
//! exists (headless CI), the injected clock drives playback.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use majik_core::video::{Frame, Source, VideoError, VideoInfo};

/// The reference clock: `BackgroundExecutor::now()` in the app, a fake in tests.
pub type Now = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Playback within this many seconds of the end counts as finished.
const END_EPSILON: f64 = 1e-6;

enum Clock {
    Audio(majik_audio::Player),
    Monotonic { now: Now, base_secs: f64, running_since: Option<Instant> },
}

/// Decoding work the app runs off the UI thread; produced by [`Player::decode_job`].
pub struct DecodeJob {
    source: Arc<Mutex<Source>>,
    target_secs: f64,
    generation: u64,
}

/// What a [`DecodeJob`] produced; feed it to [`Player::apply`].
pub struct DecodeResult {
    generation: u64,
    frame: Result<Option<Frame>, VideoError>,
}

impl DecodeJob {
    pub fn run(self) -> DecodeResult {
        let frame = match self.source.lock() {
            Ok(mut source) => source.frame_at(self.target_secs),
            Err(_) => Err(VideoError::Decode("decoder panicked".into())),
        };
        DecodeResult { generation: self.generation, frame }
    }
}

/// A single-file video player. Lives on the UI thread (`!Send` because of the audio sink); all
/// decoding goes through [`DecodeJob`]s.
pub struct Player {
    source: Arc<Mutex<Source>>,
    path: PathBuf,
    clock: Clock,
    playing: bool,
    looping: bool,
    muted: bool,
    size: (u32, u32),
    duration: f64,
    interval: Duration,
    frame: Option<Arc<Frame>>,
    /// The file has an audio track that hasn't been opened yet (see [`Self::open_audio`]).
    audio_pending: bool,
    /// Bumped by every seek so results decoded for an older position are ignored.
    generation: u64,
    /// Generation of the last result applied; `None` until the first frame is asked for.
    shown_generation: Option<u64>,
    in_flight: bool,
    error: Option<VideoError>,
}

impl Player {
    /// Wrap an already opened `Source` (open it off the UI thread — it reads the whole sample
    /// table). Starts paused at zero, looping, unmuted; cheap, as the audio sink waits for the
    /// first [`Self::play`].
    pub fn new(source: Source, path: &Path, now: Now) -> Self {
        let size = source.size();
        let duration = source.duration();
        let interval = source.frame_interval();
        let audio_pending = source.has_audio();
        Self {
            source: Arc::new(Mutex::new(source)),
            path: path.to_path_buf(),
            clock: Clock::Monotonic { now, base_secs: 0.0, running_since: None },
            playing: false,
            looping: true,
            muted: false,
            size,
            duration,
            interval,
            frame: None,
            audio_pending,
            generation: 0,
            shown_generation: None,
            in_flight: false,
            error: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the audio track drives the clock: false until the first play opens it, for silent
    /// files, and without an output device.
    pub fn has_audio(&self) -> bool {
        matches!(self.clock, Clock::Audio(_))
    }

    /// Hand the clock to the audio track, at the position reached so far; once per player, on the
    /// first play. Without an output device the file plays silently on the injected clock.
    fn open_audio(&mut self) {
        if !self.audio_pending {
            return;
        }
        self.audio_pending = false;
        match majik_audio::Player::open(&self.path) {
            Ok(mut audio) => {
                let position = self.position();
                if position > 0.0 {
                    audio.seek(position);
                }
                audio.set_volume(if self.muted { 0.0 } else { 1.0 });
                self.clock = Clock::Audio(audio);
            }
            Err(e) => tracing::warn!(target: "majik", "video audio for {}: {e:#}; playing silently", self.path.display()),
        }
    }

    pub fn play(&mut self) {
        if self.error.is_some() {
            return;
        }
        if self.at_end() {
            self.seek(0.0);
        }
        self.open_audio();
        self.playing = true;
        match &mut self.clock {
            Clock::Audio(audio) => audio.play(),
            Clock::Monotonic { now, running_since, .. } => {
                if running_since.is_none() {
                    *running_since = Some(now());
                }
            }
        }
    }

    pub fn pause(&mut self) {
        let position = self.position();
        self.playing = false;
        match &mut self.clock {
            Clock::Audio(audio) => audio.pause(),
            Clock::Monotonic { base_secs, running_since, .. } => {
                *base_secs = position;
                *running_since = None;
            }
        }
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause();
        } else {
            self.play();
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Clock::Audio(audio) = &mut self.clock {
            audio.set_volume(if muted { 0.0 } else { 1.0 });
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// Current playback position in seconds, clamped to the clip.
    pub fn position(&self) -> f64 {
        let raw = match &self.clock {
            Clock::Audio(audio) => audio.position(),
            Clock::Monotonic { now, base_secs, running_since } => {
                base_secs + running_since.map_or(0.0, |since| now().saturating_duration_since(since).as_secs_f64())
            }
        };
        raw.clamp(0.0, self.duration)
    }

    pub fn seek(&mut self, secs: f64) {
        let secs = if secs.is_finite() { secs.clamp(0.0, self.duration) } else { 0.0 };
        match &mut self.clock {
            Clock::Audio(audio) => audio.seek(secs),
            Clock::Monotonic { now, base_secs, running_since } => {
                *base_secs = secs;
                if running_since.is_some() {
                    *running_since = Some(now());
                }
            }
        }
        self.generation += 1;
    }

    /// Native pixel size.
    pub fn size(&self) -> Option<(u32, u32)> {
        (self.size.0 > 0 && self.size.1 > 0).then_some(self.size)
    }

    /// The frame that should be on screen now (none until the first decode finishes).
    pub fn frame(&self) -> Option<&Arc<Frame>> {
        self.frame.as_ref()
    }

    /// How often the app should ask for a new frame while playing.
    pub fn frame_interval(&self) -> Duration {
        self.interval
    }

    /// The decode error that stopped playback, if any.
    pub fn error(&self) -> Option<&VideoError> {
        self.error.as_ref()
    }

    /// True while the app should keep scheduling decode jobs: playing, or the picture for the
    /// current position has not been shown yet.
    pub fn wants_frames(&self) -> bool {
        self.error.is_none() && (self.playing || self.shown_generation != Some(self.generation))
    }

    /// Work for the next frame, or `None` while a job is in flight or nothing is wanted. Also
    /// where the end of the clip is handled: loop back to the start, or stop on the last frame.
    pub fn decode_job(&mut self) -> Option<DecodeJob> {
        if self.in_flight {
            return None;
        }
        if self.playing && self.at_end() {
            if self.looping {
                self.seek(0.0);
                if let Clock::Audio(audio) = &mut self.clock {
                    audio.play();
                }
            } else {
                self.pause();
            }
        }
        if !self.wants_frames() {
            return None;
        }
        self.in_flight = true;
        Some(DecodeJob { source: Arc::clone(&self.source), target_secs: self.position(), generation: self.generation })
    }

    /// Store a job's result; returns true when the frame on screen changed. Results decoded for a
    /// position from before the last seek are dropped. A decode error pauses playback and is kept
    /// in [`Self::error`]; the last good frame stays up.
    pub fn apply(&mut self, result: DecodeResult) -> bool {
        self.in_flight = false;
        if result.generation != self.generation {
            return false;
        }
        self.shown_generation = Some(self.generation);
        match result.frame {
            Ok(Some(frame)) => {
                self.frame = Some(Arc::new(frame));
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(target: "majik", "video decode for {}: {e:#}", self.path.display());
                self.pause();
                self.error = Some(e);
                false
            }
        }
    }

    fn at_end(&self) -> bool {
        match &self.clock {
            Clock::Audio(audio) => audio.finished() || self.position() >= self.duration - END_EPSILON,
            Clock::Monotonic { .. } => self.position() >= self.duration - END_EPSILON,
        }
    }
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("path", &self.path)
            .field("playing", &self.playing)
            .field("position", &self.position())
            .field("duration", &self.duration)
            .field("has_audio", &self.has_audio())
            .field("frame_pts", &self.frame.as_ref().map(|f| f.pts_secs))
            .field("error", &self.error)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use majik_core::video::encode_solid_clip;

    struct FakeClock(Arc<Mutex<Instant>>);

    impl FakeClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Instant::now())))
        }

        fn now(&self) -> Now {
            let clock = Arc::clone(&self.0);
            Arc::new(move || *clock.lock().unwrap())
        }

        fn advance(&self, secs: f64) {
            let mut now = self.0.lock().unwrap();
            *now += Duration::from_secs_f64(secs);
        }
    }

    fn clip(dir: &tempfile::TempDir, seconds: u32) -> PathBuf {
        let path = dir.path().join(format!("clip{seconds}.mp4"));
        std::fs::write(&path, encode_solid_clip(64, 48, seconds, [200, 100, 50]).unwrap()).unwrap();
        path
    }

    fn open(path: &Path, clock: &FakeClock) -> Player {
        Player::new(Source::open(path).unwrap(), path, clock.now())
    }

    /// Run the pending decode job inline, as the app's pump would on a background thread.
    fn pump(player: &mut Player) -> bool {
        match player.decode_job() {
            Some(job) => player.apply(job.run()),
            None => false,
        }
    }

    fn shown_pts(player: &Player) -> Option<f64> {
        player.frame().map(|f| f.pts_secs)
    }

    #[test]
    fn opens_paused_at_zero_with_size_and_duration() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let player = open(&clip(&dir, 3), &clock);
        assert!(!player.is_playing());
        assert_eq!(player.position(), 0.0);
        assert_eq!(player.duration(), 3.0);
        assert_eq!(player.size(), Some((64, 48)));
        assert_eq!(player.frame_interval(), Duration::from_secs(1));
        assert!(!player.has_audio(), "solid clips carry no audio track");
        assert!(player.frame().is_none());
        assert!(player.error().is_none());
    }

    #[test]
    fn first_frame_arrives_while_paused() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 2), &clock);
        assert!(player.wants_frames(), "the picture for position 0 has not been shown");
        assert!(pump(&mut player));
        assert_eq!(shown_pts(&player), Some(0.0));
        assert!(!player.wants_frames(), "paused with the right picture up: nothing more to do");
        assert!(player.decode_job().is_none());
    }

    #[test]
    fn play_pause_toggle_follow_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 3), &clock);
        player.play();
        assert!(player.is_playing());
        clock.advance(1.5);
        assert!((player.position() - 1.5).abs() < 1e-9);
        player.pause();
        clock.advance(1.0);
        assert!((player.position() - 1.5).abs() < 1e-9, "paused position holds");
        player.toggle();
        assert!(player.is_playing());
        clock.advance(0.5);
        assert!((player.position() - 2.0).abs() < 1e-9);
        player.toggle();
        assert!(!player.is_playing());
    }

    #[test]
    fn frames_advance_with_the_clock() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 3), &clock);
        player.play();
        assert!(pump(&mut player));
        assert_eq!(shown_pts(&player), Some(0.0));
        clock.advance(0.5);
        assert!(!pump(&mut player), "still within the first picture");
        clock.advance(0.5);
        assert!(pump(&mut player));
        assert_eq!(shown_pts(&player), Some(1.0));
        clock.advance(1.0);
        assert!(pump(&mut player));
        assert_eq!(shown_pts(&player), Some(2.0));
        assert!(player.wants_frames(), "playing keeps the pump alive");
    }

    #[test]
    fn only_one_job_is_in_flight_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 2), &clock);
        let job = player.decode_job().unwrap();
        assert!(player.decode_job().is_none());
        assert!(player.apply(job.run()));
        assert!(player.decode_job().is_none(), "frame shown and paused");
    }

    #[test]
    fn seek_clamps_and_ignores_results_from_before_the_seek() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 3), &clock);
        let stale = player.decode_job().unwrap();
        player.seek(10.0);
        assert_eq!(player.position(), 3.0, "clamped to the duration");
        assert!(!player.apply(stale.run()), "decoded for position 0, discarded");
        assert!(player.frame().is_none());
        player.seek(1.2);
        assert!((player.position() - 1.2).abs() < 1e-9);
        assert!(player.wants_frames());
        assert!(pump(&mut player));
        assert_eq!(shown_pts(&player), Some(1.0));
        assert!(!player.wants_frames());
        player.seek(-1.0);
        assert_eq!(player.position(), 0.0);
    }

    #[test]
    fn seeking_while_playing_keeps_the_clock_running() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 3), &clock);
        player.play();
        clock.advance(0.25);
        player.seek(2.0);
        clock.advance(0.5);
        assert!((player.position() - 2.5).abs() < 1e-9);
        assert!(player.is_playing());
    }

    #[test]
    fn looping_wraps_to_zero_and_keeps_playing() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 2), &clock);
        player.play();
        pump(&mut player);
        clock.advance(1.0);
        pump(&mut player);
        assert_eq!(shown_pts(&player), Some(1.0));
        clock.advance(1.0);
        assert_eq!(player.position(), 2.0);
        assert!(pump(&mut player), "wrapped: frame 0 is back");
        assert_eq!(shown_pts(&player), Some(0.0));
        assert!(player.position() < 1.0);
        assert!(player.is_playing());
    }

    #[test]
    fn non_looping_pauses_on_the_last_frame() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 2), &clock);
        player.set_looping(false);
        player.play();
        pump(&mut player);
        clock.advance(1.0);
        pump(&mut player);
        clock.advance(1.0);
        assert!(!pump(&mut player));
        assert!(!player.is_playing());
        assert_eq!(player.position(), 2.0);
        assert_eq!(shown_pts(&player), Some(1.0), "last picture stays up");
        player.play();
        assert_eq!(player.position(), 0.0, "play at the end restarts");
        assert!(player.is_playing());
    }

    #[test]
    fn mute_is_remembered_without_an_audio_track() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 1), &clock);
        assert!(!player.is_muted());
        player.set_muted(true);
        assert!(player.is_muted());
    }

    #[test]
    fn decode_error_pauses_and_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let clock = FakeClock::new();
        let mut player = open(&clip(&dir, 3), &clock);
        assert!(pump(&mut player));
        player.play();
        clock.advance(1.0);
        let job = player.decode_job().unwrap();
        drop(job);
        let failed = DecodeResult { generation: player.generation, frame: Err(VideoError::Decode("boom".into())) };
        assert!(!player.apply(failed));
        assert!(matches!(player.error(), Some(VideoError::Decode(_))));
        assert!(!player.is_playing());
        assert!(!player.wants_frames());
        assert_eq!(shown_pts(&player), Some(0.0), "last good frame stays up");
        player.play();
        assert!(!player.is_playing(), "a broken player does not restart");
    }
}
