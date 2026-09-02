//! Audio probing and playback for the majik desktop app.
//!
//!
//! * [`probe`] / [`duration_secs`] inspect a file with [symphonia] without
//!   touching an audio device, which makes them safe for metadata scans.
//! * [`Player`] decodes a file through [rodio]'s symphonia backend and plays
//!   it on the default output device, exposing play/pause/seek/position so a
//!   timer-driven scrubber can drive it.
//!
//! Every [`Player`] method is cheap and safe to call from the UI thread; the
//! audio itself runs on a thread owned by rodio/cpal. [`Player`] is `!Send`
//! on purpose: it owns the OS output stream, which must be dropped on the
//! thread that created it on some platforms.
//!
//! Supported containers/codecs: WAV (PCM), MP3, FLAC, AAC in MP4/M4A, and
//! Vorbis in Ogg.

use std::fs::File;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Source};
use symphonia::core::codecs::{CodecParameters, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Basic stream properties of an audio file, as reported by the demuxer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioInfo {
    /// Total duration in seconds. `0.0` if the container does not report a
    /// length and it could not be derived by scanning the packets.
    pub duration_secs: f64,
    /// Sample rate in Hz. `0` if unknown.
    pub sample_rate: u32,
    /// Channel count. `0` if unknown.
    pub channels: u16,
}

/// Inspect an audio file without opening an output device.
///
/// Uses symphonia to demux the container and read the first audio track's
/// parameters. When the container does not carry a frame count (common for
/// MP3 files without a Xing/Info header) the packets are scanned to sum their
/// durations; that is still demux-only and much cheaper than decoding.
pub fn probe(path: &Path) -> Result<AudioInfo> {
    let (mut format, track_id, params) = open_format(path)?;

    let sample_rate = params.sample_rate.unwrap_or(0);
    let mut channels = params
        .channels
        .map(|c| c.count() as u16)
        .or_else(|| {
            params
                .channel_layout
                .map(|l| l.into_channels().count() as u16)
        })
        .unwrap_or(0);

    // Preferred: the container tells us the frame count.
    let mut duration_secs = match (params.n_frames, params.time_base) {
        (Some(n), Some(tb)) => {
            let t = tb.calc_time(n);
            t.seconds as f64 + t.frac
        }
        (Some(n), None) if sample_rate > 0 => n as f64 / sample_rate as f64,
        _ => 0.0,
    };

    // Packets consumed by the checks below still count toward the duration
    // scan, so remember where they ended.
    let mut last_end_ts: u64 = 0;

    // Some containers (AAC in MP4, for one) only describe the channel layout
    // inside the codec's private config, which symphonia parses when it
    // decodes. Decoding one packet is cheap enough to get the real answer.
    if channels == 0 {
        if let Ok(mut decoder) =
            symphonia::default::get_codecs().make(&params, &DecoderOptions::default())
        {
            while let Ok(packet) = format.next_packet() {
                if packet.track_id() != track_id {
                    continue;
                }
                last_end_ts = last_end_ts.max(packet.ts().saturating_add(packet.dur()));
                match decoder.decode(&packet) {
                    Ok(buf) => {
                        channels = buf.spec().channels.count() as u16;
                        break;
                    }
                    Err(SymphoniaError::DecodeError(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    // Fallback: walk the packets and add up their timestamps (no decoding).
    if duration_secs <= 0.0 {
        if let Some(tb) = params.time_base {
            loop {
                match format.next_packet() {
                    Ok(packet) => {
                        if packet.track_id() == track_id {
                            last_end_ts = last_end_ts.max(packet.ts().saturating_add(packet.dur()));
                        }
                    }
                    // EOF is reported as an UnexpectedEof io error in symphonia 0.5.
                    Err(SymphoniaError::IoError(e))
                        if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        break
                    }
                    Err(SymphoniaError::ResetRequired) => break,
                    Err(e) => return Err(e).context("scanning packets for duration"),
                }
            }
            let t = tb.calc_time(last_end_ts);
            duration_secs = t.seconds as f64 + t.frac;
        }
    }

    Ok(AudioInfo {
        duration_secs,
        sample_rate,
        channels,
    })
}

/// Demux `path` and pick its first audio track: the reader positioned at the
/// first packet, the track's id, and its codec parameters.
fn open_format(path: &Path) -> Result<(Box<dyn FormatReader>, u32, CodecParameters)> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("unrecognized audio format: {}", path.display()))?;
    let format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    Ok((format, track_id, params))
}

/// Duration of an audio file in seconds. Convenience wrapper over [`probe`].
pub fn duration_secs(path: &Path) -> Result<f64> {
    Ok(probe(path)?.duration_secs)
}

/// How many of the track's packets [`ensure_decodable`] tries before giving up on it.
const DECODE_PROBE_PACKETS: usize = 4;

/// Check that the file's audio track decodes, not merely demuxes.
///
/// symphonia reads AAC-LC only. A stream whose frames carry more channels than
/// its configuration declares, which some providers' clips do, fails every
/// frame; rodio then skips every packet and plays silence while the decoder
/// logs an error for each one. Trying the first few packets here lets a caller
/// play the picture without sound and say why, once.
pub fn ensure_decodable(path: &Path) -> Result<()> {
    let (mut format, track_id, params) = open_format(path)?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| anyhow!("no decoder for the audio track of {}: {e}", path.display()))?;

    let mut tried = 0;
    let mut last_failure = None;
    while tried < DECODE_PROBE_PACKETS {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(e).with_context(|| format!("reading audio packets of {}", path.display())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        tried += 1;
        match decoder.decode(&packet) {
            Ok(_) => return Ok(()),
            // Malformed data and a feature the decoder lacks (SBR, a program config element) both
            // leave the frame unplayed; the next packet may still say more.
            Err(SymphoniaError::DecodeError(reason)) | Err(SymphoniaError::Unsupported(reason)) => {
                last_failure = Some(reason)
            }
            Err(e) => return Err(e).with_context(|| format!("decoding the audio track of {}", path.display())),
        }
    }
    match last_failure {
        Some(reason) => Err(anyhow!(
            "the audio track of {} can't be decoded ({reason}); none of its first {tried} packets did",
            path.display()
        )),
        // A track without packets has nothing to play and nothing to fail on.
        None => Ok(()),
    }
}

/// Returns `true` if a default audio output device can be opened.
///
/// Handy for tests and headless environments: opening a [`Player`] will fail
/// with an error when this returns `false`.
pub fn output_device_available() -> bool {
    match DeviceSinkBuilder::open_default_sink() {
        Ok(mut sink) => {
            // Closing the probe sink again is the point; rodio would announce it on stderr.
            sink.log_on_drop(false);
            true
        }
        Err(_) => false,
    }
}

/// A single-file audio player bound to the default output device.
///
/// [`open`](Player::open) loads the file
/// paused at position zero; [`play`](Player::play) starts or resumes (and
/// restarts from the top after the track has [`finished`](Player::finished));
/// [`stop`](Player::stop) unloads and rewinds to zero.
///
/// The type is intentionally `!Send`; keep it on the UI thread.
///
/// Every load, including a seek, gets a fresh rodio sink rather than reusing
/// one: rodio's `Player::append` waits for the audio thread to drain a stopped
/// queue, and its `try_seek` waits for the audio thread to answer, and both
/// wait forever once the sound has ended (nothing runs the callback that
/// would answer) or the device has stopped pulling. The end of a clip, when
/// the video player asks to loop, is exactly when a sound has just ended, and
/// the UI thread froze there. Dropping a sink only sets a flag.
pub struct Player {
    path: PathBuf,
    /// Queue/controls for the current source. Declared before `_stream` so it
    /// is dropped first (fields drop in declaration order).
    sink: rodio::Player,
    /// Owns the cpal output stream; playback stops when this is dropped.
    _stream: MixerDeviceSink,
    info: AudioInfo,
    volume: f32,
    /// A source has been appended since the last stop.
    loaded: bool,
    /// Seconds skipped at decode time when the source could not seek natively.
    /// Added to the sink's reported position.
    skip_offset: f64,
    _not_send: PhantomData<*const ()>,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("path", &self.path)
            .field("info", &self.info)
            .field("volume", &self.volume)
            .field("loaded", &self.loaded)
            .field("playing", &self.is_playing())
            .field("position", &self.position())
            .finish()
    }
}

impl Player {
    /// Open `path`, probe it, and prepare it on the default output device.
    ///
    /// The player starts paused at position zero. Fails if the file cannot be
    /// decoded or no output device is available.
    pub fn open(path: &Path) -> Result<Self> {
        let info = probe(path)?;
        ensure_decodable(path)?;

        let mut stream = DeviceSinkBuilder::open_default_sink()
            .map_err(|e| anyhow!("open default audio output: {e}"))?;
        // The stream closes with the player, on purpose; rodio would announce it on stderr.
        stream.log_on_drop(false);
        let sink = fresh_sink(&stream, 1.0);

        let mut player = Self {
            path: path.to_path_buf(),
            _stream: stream,
            sink,
            info,
            volume: 1.0,
            loaded: false,
            skip_offset: 0.0,
            _not_send: PhantomData,
        };
        player.load(0.0)?;
        Ok(player)
    }

    /// Stream properties reported by [`probe`] at open time.
    pub fn info(&self) -> AudioInfo {
        self.info
    }

    /// Path this player was opened with.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Start or resume playback. After the track has finished, or after
    /// [`stop`](Player::stop), playback restarts from the beginning.
    pub fn play(&mut self) {
        if !self.loaded || self.sink.empty() {
            if let Err(e) = self.load(0.0) {
                tracing::error!(path = %self.path.display(), error = %e, "reload audio source");
                return;
            }
        }
        self.sink.play();
    }

    /// Pause playback, keeping the current position.
    pub fn pause(&mut self) {
        self.sink.pause();
    }

    /// [`pause`](Player::pause) if playing, otherwise [`play`](Player::play).
    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Stop playback and rewind to zero. The next [`play`](Player::play)
    /// starts from the beginning.
    pub fn stop(&mut self) {
        self.sink.stop();
        self.sink.pause();
        self.loaded = false;
        self.skip_offset = 0.0;
    }

    /// `true` while audio is actually being produced (loaded, not paused,
    /// and not yet at the end).
    pub fn is_playing(&self) -> bool {
        self.loaded && !self.sink.is_paused() && !self.sink.empty()
    }

    /// Current playback position in seconds. Advances while playing and is
    /// accurate immediately after [`seek`](Player::seek).
    pub fn position(&self) -> f64 {
        if !self.loaded {
            return 0.0;
        }
        let pos = self.sink.get_pos().as_secs_f64() + self.skip_offset;
        let duration = self.duration();
        if duration > 0.0 {
            pos.min(duration)
        } else {
            pos
        }
    }

    /// Total duration in seconds (from [`probe`]).
    pub fn duration(&self) -> f64 {
        self.info.duration_secs
    }

    /// Seek to `secs` (clamped to `[0, duration]`), preserving the
    /// play/pause state. If the track was stopped or had finished, the source
    /// is reloaded paused at the requested position.
    ///
    /// Always reopens the file with the decoder positioned at `secs` (see the
    /// type's note): the decoder's own seek runs on this thread and never
    /// waits on the audio one.
    pub fn seek(&mut self, secs: f64) {
        let duration = self.duration();
        let target = if secs.is_finite() { secs.max(0.0) } else { 0.0 };
        let target = if duration > 0.0 {
            target.min(duration)
        } else {
            target
        };

        let was_playing = self.is_playing();
        if let Err(e) = self.load(target) {
            tracing::error!(path = %self.path.display(), error = %e, "reload for seek");
            return;
        }
        if was_playing {
            self.sink.play();
        }
    }

    /// Set the output volume; `1.0` is unity gain. Values are clamped to
    /// `[0, 1]` and persist across reloads.
    pub fn set_volume(&mut self, v: f32) {
        let v = if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            1.0
        };
        self.volume = v;
        self.sink.set_volume(v as rodio::Float);
    }

    /// Current volume as last set by [`set_volume`](Player::set_volume).
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// `true` once the track has played through to the end (and has not been
    /// stopped or restarted since). Lets the UI flip its play/pause control
    /// back to "play".
    pub fn finished(&self) -> bool {
        self.loaded && self.sink.empty()
    }

    /// Decode the file and put it on a fresh sink, paused at `start_secs`;
    /// the previous sink, and whatever it was playing, is dropped.
    fn load(&mut self, start_secs: f64) -> Result<()> {
        self.loaded = false;
        self.skip_offset = 0.0;

        let file =
            File::open(&self.path).with_context(|| format!("open {}", self.path.display()))?;
        let byte_len = file.metadata().ok().map(|m| m.len());

        let mut builder = Decoder::builder()
            .with_data(file)
            .with_seekable(true)
            .with_gapless(true);
        if let Some(len) = byte_len {
            builder = builder.with_byte_len(len);
        }
        if let Some(ext) = self.path.extension().and_then(|e| e.to_str()) {
            builder = builder.with_hint(ext);
        }
        let mut decoder = builder
            .build()
            .with_context(|| format!("decode {}", self.path.display()))?;

        // A new sink's queue is empty, so `append` has nothing to wait for.
        let sink = fresh_sink(&self._stream, self.volume);
        // Seek the bare decoder before it enters the sink so the first
        // samples already come from the right place. rodio's position tracker
        // starts at zero for the new source, so the pre-seek is accounted for
        // in `skip_offset`.
        if start_secs > 0.0 {
            self.skip_offset = start_secs;
            match decoder.try_seek(Duration::from_secs_f64(start_secs)) {
                Ok(()) => sink.append(decoder),
                Err(e) => {
                    tracing::debug!(error = %e, "decoder seek failed; skipping instead");
                    sink.append(decoder.skip_duration(Duration::from_secs_f64(start_secs)));
                }
            }
        } else {
            sink.append(decoder);
        }
        self.sink = sink;
        self.loaded = true;
        Ok(())
    }
}

/// A paused sink on `stream` at `volume`, with nothing queued.
fn fresh_sink(stream: &MixerDeviceSink, volume: f32) -> rodio::Player {
    let sink = rodio::Player::connect_new(stream.mixer());
    // Paused so a freshly appended source does not auto-play.
    sink.pause();
    sink.set_volume(volume as rodio::Float);
    sink
}
