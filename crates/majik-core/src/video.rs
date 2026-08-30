//! In-process video: MP4 demux with `re_mp4` and H.264 decode with `openh264` (vendored, compiled
//! from source), so probing, poster frames and playback need no ffmpeg on the machine and behave the
//! same on macOS, Windows and Linux. The encoder half (`encode_solid_clip`) exists for the Mock
//! provider and for tests, which is why it lives here rather than in `majik-providers`.
//!
//! Scope: H.264 (`avc1`) in ISO-BMFF with an optional AAC track, which is what every provider
//! returns.
//! Anything else is [`VideoError::UnsupportedCodec`] rather than a silent failure.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use image::imageops::FilterType;
use openh264::encoder::{Encoder, EncoderConfig, FrameRate, IntraFramePeriod};
use openh264::formats::YUVBuffer;
use openh264::{OpenH264API, Timestamp};
use re_mp4::{StsdBoxContent, TrackKind};

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("no video track")]
    NoVideoTrack,
    #[error("unsupported codec ({fourcc})")]
    UnsupportedCodec { fourcc: String },
    #[error("invalid video file: {0}")]
    Container(String),
    #[error("decode failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<re_mp4::Error> for VideoError {
    fn from(e: re_mp4::Error) -> Self {
        VideoError::Container(e.to_string())
    }
}

impl From<openh264::Error> for VideoError {
    fn from(e: openh264::Error) -> Self {
        VideoError::Decode(e.to_string())
    }
}

impl From<mp4::Error> for VideoError {
    fn from(e: mp4::Error) -> Self {
        VideoError::Container(e.to_string())
    }
}

/// Container-level facts about a video file (no decoding involved).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VideoInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: Option<f64>,
    pub has_audio: bool,
}

/// One decoded picture in BGRA8 — the byte order GPUI's texture atlas wants — tightly packed
/// (`width * 4` bytes per row).
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pts_secs: f64,
    pub bgra: Vec<u8>,
}

impl Frame {
    /// The frame as a true-RGBA image (for encoding posters and for tests).
    pub fn to_rgba_image(&self) -> image::RgbaImage {
        let mut rgba = self.bgra.clone();
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        // Length is `width * height * 4` by construction, so `from_raw` cannot fail.
        image::RgbaImage::from_raw(self.width, self.height, rgba).unwrap_or_default()
    }
}

/// The poster frame is taken this far into the clip.
const POSTER_SECS: f64 = 0.1;

/// Upper bound on samples fed per `frame_at` call so a scrub across a long GOP cannot stall the
/// caller; the next call carries on from where this one stopped.
const MAX_SAMPLES_PER_CALL: usize = 64;

struct SampleIndex {
    offset: u64,
    size: usize,
    cts_secs: f64,
    is_sync: bool,
}

struct Demuxed {
    samples: Vec<SampleIndex>,
    /// `(cts_secs, sample index)` of every sync sample, in decode order.
    sync_points: Vec<(f64, usize)>,
    /// SPS and PPS as Annex-B, fed before every sync sample.
    parameter_sets: Vec<u8>,
    nal_length_size: usize,
    width: u32,
    height: u32,
    duration_secs: f64,
    frame_interval: Duration,
    has_audio: bool,
}

fn demux(path: &Path) -> Result<(BufReader<File>, Demuxed), VideoError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mp4 = re_mp4::Mp4::read(&mut reader, len)?;
    if !mp4.moofs.is_empty() {
        return Err(VideoError::Container("fragmented MP4 is not supported".into()));
    }
    let has_audio = mp4.tracks().values().any(|t| t.kind == Some(TrackKind::Audio));
    // `kind` is derived from the codec, so a video track with a codec re_mp4 does not know has
    // `kind: None`; the handler box still says it is video, and we want to report the codec.
    let track = mp4
        .tracks()
        .values()
        .find(|t| t.kind == Some(TrackKind::Video) || t.trak(&mp4).mdia.hdlr.handler_type.value == *b"vide")
        .ok_or(VideoError::NoVideoTrack)?;
    let stsd = &track.trak(&mp4).mdia.minf.stbl.stsd.contents;
    let avc1 = match stsd {
        StsdBoxContent::Avc1(avc1) => avc1,
        other => return Err(VideoError::UnsupportedCodec { fourcc: stsd_fourcc(other) }),
    };
    let avcc = &avc1.avcc;
    let mut parameter_sets = Vec::new();
    for nal in &avcc.sequence_parameter_sets {
        parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
        parameter_sets.extend_from_slice(&sps::widen_dpb(&nal.bytes)?);
    }
    for nal in &avcc.picture_parameter_sets {
        parameter_sets.extend_from_slice(&[0, 0, 0, 1]);
        parameter_sets.extend_from_slice(&nal.bytes);
    }
    if parameter_sets.is_empty() {
        return Err(VideoError::Container("avcC carries no parameter sets".into()));
    }
    let nal_length_size = usize::from(avcc.length_size_minus_one) + 1;

    let timescale = track.timescale.max(1) as f64;
    let samples: Vec<SampleIndex> = track
        .samples
        .iter()
        .map(|s| SampleIndex {
            offset: s.offset,
            size: s.size as usize,
            cts_secs: s.composition_timestamp as f64 / timescale,
            is_sync: s.is_sync,
        })
        .collect();
    if samples.is_empty() {
        return Err(VideoError::Container("video track has no samples".into()));
    }
    let sync_points: Vec<(f64, usize)> = samples.iter().enumerate().filter(|(_, s)| s.is_sync).map(|(i, s)| (s.cts_secs, i)).collect();

    let mvhd = &mp4.moov.mvhd;
    let duration_secs = if mvhd.duration > 0 && mvhd.timescale > 0 {
        mvhd.duration as f64 / f64::from(mvhd.timescale)
    } else {
        track.duration as f64 / timescale
    };
    let (width, height) = if avc1.width > 0 && avc1.height > 0 {
        (u32::from(avc1.width), u32::from(avc1.height))
    } else {
        (u32::from(track.width), u32::from(track.height))
    };
    let mut durations: Vec<u64> = track.samples.iter().map(|s| s.duration).collect();
    durations.sort_unstable();
    let median = durations.get(durations.len() / 2).copied().unwrap_or(0) as f64 / timescale;
    let frame_interval = Duration::from_secs_f64(if median > 0.0 { median.clamp(1.0 / 120.0, 1.0) } else { 1.0 / 30.0 });

    Ok((
        reader,
        Demuxed { samples, sync_points, parameter_sets, nal_length_size, width, height, duration_secs, frame_interval, has_audio },
    ))
}

fn stsd_fourcc(content: &StsdBoxContent) -> String {
    match content {
        StsdBoxContent::Av01(_) => "av01".into(),
        StsdBoxContent::Avc1(_) => "avc1".into(),
        StsdBoxContent::Hvc1(_) => "hvc1".into(),
        StsdBoxContent::Hev1(_) => "hev1".into(),
        StsdBoxContent::Vp08(_) => "vp08".into(),
        StsdBoxContent::Vp09(_) => "vp09".into(),
        StsdBoxContent::Mp4a(_) => "mp4a".into(),
        StsdBoxContent::Tx3g(_) => "tx3g".into(),
        StsdBoxContent::Unknown(fourcc) => fourcc.to_string(),
    }
}

/// Container facts only; never decodes, so it is cheap enough to run when a generation completes.
pub fn probe(path: &Path) -> Result<VideoInfo, VideoError> {
    let (_, d) = demux(path)?;
    Ok(VideoInfo {
        width: (d.width > 0).then_some(d.width),
        height: (d.height > 0).then_some(d.height),
        duration_secs: (d.duration_secs > 0.0).then_some(d.duration_secs),
        has_audio: d.has_audio,
    })
}

/// The frame at ≈0.1 s (or the last one for shorter clips) fitted inside `max_dim`, never upscaled.
pub fn poster(path: &Path, max_dim: u32) -> Result<image::RgbaImage, VideoError> {
    let mut source = Source::open(path)?;
    let frame = match source.frame_at(POSTER_SECS)? {
        Some(frame) => frame,
        // The first picture starts after the poster time (edit lists): take whatever is last.
        None => source.frame_at(source.duration())?.ok_or_else(|| VideoError::Decode("no picture decoded".into()))?,
    };
    let image = frame.to_rgba_image();
    if frame.width.max(frame.height) <= max_dim {
        return Ok(image);
    }
    Ok(image::DynamicImage::ImageRgba8(image).resize(max_dim, max_dim, FilterType::Triangle).into_rgba8())
}

/// Demuxer plus decoder over one file. `Send` so the app can drive it from a background thread;
/// not `Sync` — share it behind a mutex.
pub struct Source {
    file: BufReader<File>,
    demuxed: Demuxed,
    decoder: h264::Decoder,
    /// Next sample (decode order) to feed.
    next_sample: usize,
    /// Composition times of fed samples whose pictures have not come out yet; pictures leave the
    /// decoder in display order, so the smallest pending time is the next picture's pts.
    pending_cts: BinaryHeap<Reverse<OrderedSecs>>,
    /// Decoded pictures not yet handed out (the reordering look-ahead and end-of-stream flush).
    ready: VecDeque<Frame>,
    last_pts: Option<f64>,
    started: bool,
    at_end: bool,
    sample_bytes: Vec<u8>,
    annex_b: Vec<u8>,
}

/// `f64` seconds with a total order for the heap (timestamps are never NaN).
#[derive(Clone, Copy, PartialEq)]
struct OrderedSecs(f64);

impl Eq for OrderedSecs {}

impl PartialOrd for OrderedSecs {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedSecs {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Source {
    pub fn open(path: &Path) -> Result<Self, VideoError> {
        let (file, demuxed) = demux(path)?;
        Ok(Self {
            file,
            demuxed,
            decoder: new_decoder()?,
            next_sample: 0,
            pending_cts: BinaryHeap::new(),
            ready: VecDeque::new(),
            last_pts: None,
            started: false,
            at_end: false,
            sample_bytes: Vec::new(),
            annex_b: Vec::new(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.demuxed.width, self.demuxed.height)
    }

    pub fn duration(&self) -> f64 {
        self.demuxed.duration_secs
    }

    pub fn has_audio(&self) -> bool {
        self.demuxed.has_audio
    }

    /// Median sample duration, clamped to 1/120..=1 s: how often a player needs to ask for frames.
    pub fn frame_interval(&self) -> Duration {
        self.demuxed.frame_interval
    }

    pub fn info(&self) -> VideoInfo {
        let d = &self.demuxed;
        VideoInfo {
            width: (d.width > 0).then_some(d.width),
            height: (d.height > 0).then_some(d.height),
            duration_secs: (d.duration_secs > 0.0).then_some(d.duration_secs),
            has_audio: d.has_audio,
        }
    }

    /// The picture that should be on screen at `target_secs`. Decodes forward from the current
    /// position; a target before the last returned picture, or past a later sync sample, restarts
    /// from the nearest preceding sync sample. `Ok(None)` means the picture already returned is
    /// still the right one (or the stream is exhausted).
    pub fn frame_at(&mut self, target_secs: f64) -> Result<Option<Frame>, VideoError> {
        let target = if target_secs.is_finite() { target_secs.max(0.0) } else { 0.0 };
        let start = self.sync_index_for(target);
        let restart = match self.last_pts {
            _ if !self.started => start > 0,
            Some(last) => target < last || start > self.next_sample,
            None => start > self.next_sample,
        };
        if restart {
            self.restart_at(start)?;
        }
        self.started = true;

        let mut best: Option<Frame> = None;
        let mut fed = 0;
        loop {
            while self.ready.front().is_some_and(|f| f.pts_secs <= target) {
                best = self.ready.pop_front();
            }
            if !self.ready.is_empty() || self.at_end || fed >= MAX_SAMPLES_PER_CALL {
                break;
            }
            self.decode_next()?;
            fed += 1;
        }

        match best {
            Some(frame) if self.last_pts != Some(frame.pts_secs) => {
                self.last_pts = Some(frame.pts_secs);
                Ok(Some(frame))
            }
            _ => Ok(None),
        }
    }

    /// Index of the last sync sample whose composition time is at or before `target`.
    fn sync_index_for(&self, target: f64) -> usize {
        let points = &self.demuxed.sync_points;
        let n = points.partition_point(|(cts, _)| *cts <= target);
        if n == 0 {
            0
        } else {
            points[n - 1].1
        }
    }

    fn restart_at(&mut self, sample: usize) -> Result<(), VideoError> {
        self.decoder = new_decoder()?;
        self.pending_cts.clear();
        self.ready.clear();
        self.next_sample = sample;
        self.at_end = false;
        self.last_pts = None;
        Ok(())
    }

    /// Feed the next sample (or flush at the end of the stream) and queue any pictures produced.
    fn decode_next(&mut self) -> Result<(), VideoError> {
        if self.next_sample >= self.demuxed.samples.len() {
            self.at_end = true;
            self.decoder.end_of_stream();
            while self.decoder.buffered_pictures() > 0 {
                let Some(picture) = self.decoder.flush()? else { break };
                let pts = self.pending_cts.pop().map(|Reverse(t)| t.0).unwrap_or(0.0);
                self.ready.push_back(frame_from_picture(&picture, pts));
            }
            return Ok(());
        }
        let index = self.next_sample;
        self.next_sample += 1;
        self.load_sample(index)?;
        self.pending_cts.push(Reverse(OrderedSecs(self.demuxed.samples[index].cts_secs)));
        if let Some(picture) = self.decoder.decode(&self.annex_b)? {
            let pts = self.pending_cts.pop().map(|Reverse(t)| t.0).unwrap_or(0.0);
            self.ready.push_back(frame_from_picture(&picture, pts));
        }
        Ok(())
    }

    /// Read one sample and rewrite its AVCC (length-prefixed) NAL units as Annex-B into `annex_b`,
    /// with SPS/PPS in front of sync samples.
    fn load_sample(&mut self, index: usize) -> Result<(), VideoError> {
        let sample = &self.demuxed.samples[index];
        self.file.seek(SeekFrom::Start(sample.offset))?;
        self.sample_bytes.resize(sample.size, 0);
        self.file.read_exact(&mut self.sample_bytes)?;
        self.annex_b.clear();
        if sample.is_sync {
            self.annex_b.extend_from_slice(&self.demuxed.parameter_sets);
        }
        avcc_to_annex_b(&self.sample_bytes, self.demuxed.nal_length_size, &mut self.annex_b)
    }
}

fn new_decoder() -> Result<h264::Decoder, VideoError> {
    h264::Decoder::new(h264::DECODER_THREADS)
}

fn avcc_to_annex_b(sample: &[u8], nal_length_size: usize, out: &mut Vec<u8>) -> Result<(), VideoError> {
    let mut pos = 0;
    while pos < sample.len() {
        let Some(prefix) = sample.get(pos..pos + nal_length_size) else {
            return Err(VideoError::Container("truncated NAL length".into()));
        };
        let len = prefix.iter().fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        pos += nal_length_size;
        let Some(nal) = sample.get(pos..pos + len) else {
            return Err(VideoError::Container("NAL unit overruns its sample".into()));
        };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        pos += len;
    }
    Ok(())
}

fn frame_from_picture(picture: &h264::Picture<'_>, pts_secs: f64) -> Frame {
    let (width, height) = (picture.width, picture.height);
    let mut bgra = vec![0u8; width * height * 4];
    yuv420_to_bgra(picture, &mut bgra);
    Frame { width: width as u32, height: height as u32, pts_secs, bgra }
}

/// BT.601 limited-range Y'CbCr 4:2:0 → BGRA8 (alpha 255), the inverse of [`rgb_to_yuv`].
fn yuv420_to_bgra(picture: &h264::Picture<'_>, out: &mut [u8]) {
    let (width, height) = (picture.width, picture.height);
    let (y_stride, uv_stride) = picture.strides;
    for row in 0..height {
        let y_row = &picture.y[row * y_stride..][..width];
        let u_row = &picture.u[(row / 2) * uv_stride..][..width.div_ceil(2)];
        let v_row = &picture.v[(row / 2) * uv_stride..][..width.div_ceil(2)];
        let out_row = &mut out[row * width * 4..][..width * 4];
        for (x, px) in out_row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let c = 298 * (i32::from(y_row[x]) - 16);
            let d = i32::from(u_row[x / 2]) - 128;
            let e = i32::from(v_row[x / 2]) - 128;
            px[0] = ((c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
            px[1] = ((c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            px[2] = ((c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            px[3] = 255;
        }
    }
}

/// A solid-colour H.264 clip at one frame per second, every frame an IDR: what the Mock provider
/// returns and what tests decode.
pub fn encode_solid_clip(width: u32, height: u32, seconds: u32, rgb: [u8; 3]) -> Result<Vec<u8>, VideoError> {
    encode_clip(width, height, seconds.max(1), 1, 1, rgb)
}

/// General form of [`encode_solid_clip`]: `frames` pictures at `fps`, a keyframe every
/// `keyframe_every` frames (1 = all-intra).
pub fn encode_clip(width: u32, height: u32, frames: u32, fps: u32, keyframe_every: u32, rgb: [u8; 3]) -> Result<Vec<u8>, VideoError> {
    let width = (width.max(2) & !1) as usize;
    let height = (height.max(2) & !1) as usize;
    let fps = fps.max(1);
    let keyframe_every = keyframe_every.max(1);

    let (y, u, v) = rgb_to_yuv(rgb);
    let luma = width * height;
    let mut planes = vec![y; luma * 3 / 2];
    planes[luma..luma + luma / 4].fill(u);
    planes[luma + luma / 4..].fill(v);
    let picture = YUVBuffer::from_vec(planes, width, height);

    let config = EncoderConfig::new()
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(keyframe_every));
    let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;

    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut samples: Vec<(Vec<u8>, bool)> = Vec::with_capacity(frames as usize);
    for i in 0..frames {
        if i % keyframe_every == 0 {
            encoder.force_intra_frame();
        }
        let stream = encoder.encode_at(&picture, Timestamp::from_millis(u64::from(i) * 1000 / u64::from(fps)))?;
        let mut sample = Vec::new();
        let mut is_sync = false;
        for layer in (0..stream.num_layers()).filter_map(|l| stream.layer(l)) {
            for nal in (0..layer.nal_count()).filter_map(|n| layer.nal_unit(n)) {
                let nal = strip_start_code(nal);
                let Some(&header) = nal.first() else { continue };
                match header & 0x1f {
                    7 => sps.get_or_insert_with(|| nal.to_vec()),
                    8 => pps.get_or_insert_with(|| nal.to_vec()),
                    9 => continue,
                    kind => {
                        is_sync |= kind == 5;
                        sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        sample.extend_from_slice(nal);
                        continue;
                    }
                };
            }
        }
        samples.push((sample, is_sync));
    }
    let (Some(sps), Some(pps)) = (sps, pps) else {
        return Err(VideoError::Decode("encoder produced no parameter sets".into()));
    };

    let brand = |b: &[u8; 4]| mp4::FourCC { value: *b };
    let mut writer = mp4::Mp4Writer::write_start(
        Cursor::new(Vec::new()),
        &mp4::Mp4Config {
            major_brand: brand(b"isom"),
            minor_version: 512,
            compatible_brands: vec![brand(b"isom"), brand(b"iso2"), brand(b"avc1"), brand(b"mp41")],
            timescale: 1000,
        },
    )?;
    writer.add_track(&mp4::TrackConfig {
        track_type: mp4::TrackType::Video,
        timescale: 1000,
        language: "und".into(),
        media_conf: mp4::MediaConfig::AvcConfig(mp4::AvcConfig { width: width as u16, height: height as u16, seq_param_set: sps, pic_param_set: pps }),
    })?;
    let frame_ms = 1000 / u64::from(fps);
    for (i, (bytes, is_sync)) in samples.into_iter().enumerate() {
        writer.write_sample(
            1,
            &mp4::Mp4Sample { start_time: i as u64 * frame_ms, duration: frame_ms as u32, rendering_offset: 0, is_sync, bytes: bytes.into() },
        )?;
    }
    writer.write_end()?;
    Ok(writer.into_writer().into_inner())
}

fn strip_start_code(nal: &[u8]) -> &[u8] {
    if let Some(rest) = nal.strip_prefix(&[0, 0, 0, 1]) {
        rest
    } else if let Some(rest) = nal.strip_prefix(&[0, 0, 1]) {
        rest
    } else {
        nal
    }
}

/// BT.601 limited-range RGB → Y'CbCr, the default interpretation decoders apply to 4:2:0 SD content.
fn rgb_to_yuv([r, g, b]: [u8; 3]) -> (u8, u8, u8) {
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let y = 16.0 + (65.738 * r + 129.057 * g + 25.064 * b) / 256.0;
    let u = 128.0 + (-37.945 * r - 74.494 * g + 112.439 * b) / 256.0;
    let v = 128.0 + (112.439 * r - 94.154 * g - 18.285 * b) / 256.0;
    (y.round() as u8, u.round() as u8, v.round() as u8)
}

/// Thin wrapper over openh264's C decoder API, kept so the decoder can be told when the stream
/// ends (`DECODER_OPTION_END_OF_STREAM`, which the `openh264` crate's `Decoder` never sets — without
/// it `FlushFrame` refuses to release the pictures held back for B-frame reordering).
mod h264 {
    use std::os::raw::{c_int, c_long, c_void};
    use std::ptr::{addr_of_mut, null_mut};

    use openh264_sys2::{
        dsBitstreamError, dsInitialOptExpected, dsInvalidArgument, dsNoParamSets, dsOutOfMemory, DynamicAPI, ISVCDecoder,
        ISVCDecoderVtbl, SBufferInfo, SDecodingParam, SVideoProperty, API, DECODER_OPTION_END_OF_STREAM,
        DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER, DECODER_OPTION_NUM_OF_THREADS, DECODING_STATE, ERROR_CON_DISABLE,
    };

    use super::VideoError;

    /// openh264's threaded mode hands out pictures that are still being written and can deadlock at
    /// end of stream, so decoding stays single-threaded; the picture-pool problem that threads would
    /// have solved is handled by [`super::sps::widen_dpb`] instead.
    pub const DECODER_THREADS: c_int = 0;

    /// Decoding states that mean the picture (or the whole stream) is lost, as opposed to
    /// informational bits such as "frame pending".
    const FATAL_STATES: DECODING_STATE = dsBitstreamError | dsNoParamSets | dsInvalidArgument | dsInitialOptExpected | dsOutOfMemory;

    /// A decoded I420 picture borrowed from the decoder's output buffer (valid until the next call).
    pub struct Picture<'a> {
        pub width: usize,
        pub height: usize,
        /// `(luma, chroma)` row pitches in bytes.
        pub strides: (usize, usize),
        pub y: &'a [u8],
        pub u: &'a [u8],
        pub v: &'a [u8],
    }

    pub struct Decoder {
        api: DynamicAPI,
        decoder: *mut ISVCDecoder,
        initialized: bool,
    }

    // SAFETY: the decoder is only ever driven from one thread at a time (`Source` is `Send`, not
    // `Sync`); openh264 has no thread-affinity requirements for its API calls.
    unsafe impl Send for Decoder {}

    impl Decoder {
        pub fn new(threads: c_int) -> Result<Self, VideoError> {
            let api = DynamicAPI::from_source();
            let mut decoder: *mut ISVCDecoder = null_mut();
            // SAFETY: plain FFI construction; a non-zero return or null pointer is checked below.
            let created = unsafe { api.WelsCreateDecoder(&mut decoder) };
            if created != 0 || decoder.is_null() {
                return Err(VideoError::Decode(format!("WelsCreateDecoder failed ({created})")));
            }
            let mut this = Self { api, decoder, initialized: false };
            let mut threads = threads;
            // SAFETY: `threads` outlives the call; the option is documented to take an `int`.
            unsafe { this.set_option(DECODER_OPTION_NUM_OF_THREADS, addr_of_mut!(threads).cast()) };
            let params = SDecodingParam {
                pFileNameRestructed: null_mut(),
                uiCpuLoad: 0,
                uiTargetDqLayer: 0,
                eEcActiveIdc: ERROR_CON_DISABLE,
                bParseOnly: false,
                sVideoProperty: SVideoProperty { size: 0, eVideoBsType: 0 },
            };
            // SAFETY: `params` is a fully initialised C struct that outlives the call.
            let initialized = unsafe { this.vtable().Initialize.map(|f| f(this.decoder, &params)) };
            match initialized {
                Some(0) => {
                    this.initialized = true;
                    Ok(this)
                }
                Some(code) => Err(VideoError::Decode(format!("decoder initialise failed ({code})"))),
                None => Err(VideoError::Decode("decoder vtable is missing Initialize".into())),
            }
        }

        fn vtable(&self) -> &ISVCDecoderVtbl {
            // SAFETY: `decoder` is a valid `ISVCDecoder*` (a pointer to a vtable pointer) for the
            // lifetime of `self`; openh264 never replaces the vtable.
            unsafe { &**self.decoder }
        }

        unsafe fn set_option(&mut self, option: c_int, value: *mut c_void) -> c_long {
            // SAFETY: the caller guarantees `value` points at the type the option expects.
            unsafe { self.vtable().SetOption.map_or(-1, |f| f(self.decoder, option, value)) }
        }

        /// Feed one access unit (Annex-B) and return the picture that became displayable, if any.
        pub fn decode(&mut self, packet: &[u8]) -> Result<Option<Picture<'_>>, VideoError> {
            let mut dst = [null_mut::<u8>(); 3];
            let mut info = SBufferInfo::default();
            let Some(decode) = self.vtable().DecodeFrameNoDelay else {
                return Err(VideoError::Decode("decoder vtable is missing DecodeFrameNoDelay".into()));
            };
            // SAFETY: `packet` is alive for the call; `dst`/`info` are valid out-parameters.
            let state = unsafe { decode(self.decoder, packet.as_ptr(), packet.len() as c_int, dst.as_mut_ptr(), &mut info) };
            if state & FATAL_STATES != 0 {
                return Err(VideoError::Decode(format!("openh264 decoding state {state:#x}")));
            }
            Ok(picture(&dst, &info))
        }

        /// Tell the decoder no more data is coming, so `flush` may release everything it holds.
        pub fn end_of_stream(&mut self) {
            let mut flag: c_int = 1;
            // SAFETY: `flag` outlives the call; the option takes an `int`.
            unsafe { self.set_option(DECODER_OPTION_END_OF_STREAM, addr_of_mut!(flag).cast()) };
        }

        /// Pictures decoded but still held back for reordering.
        pub fn buffered_pictures(&mut self) -> usize {
            let mut count: c_int = 0;
            // SAFETY: `count` outlives the call; the option writes an `int`.
            let rc = unsafe {
                self.vtable().GetOption.map_or(-1, |f| f(self.decoder, DECODER_OPTION_NUM_OF_FRAMES_REMAINING_IN_BUFFER, addr_of_mut!(count).cast()))
            };
            if rc == 0 {
                count.max(0) as usize
            } else {
                0
            }
        }

        /// Release the next held-back picture (display order) after `end_of_stream`.
        pub fn flush(&mut self) -> Result<Option<Picture<'_>>, VideoError> {
            let mut dst = [null_mut::<u8>(); 3];
            let mut info = SBufferInfo::default();
            let Some(flush) = self.vtable().FlushFrame else {
                return Err(VideoError::Decode("decoder vtable is missing FlushFrame".into()));
            };
            // SAFETY: `dst`/`info` are valid out-parameters.
            let state = unsafe { flush(self.decoder, dst.as_mut_ptr(), &mut info) };
            if state & FATAL_STATES != 0 {
                return Err(VideoError::Decode(format!("openh264 flush state {state:#x}")));
            }
            Ok(picture(&dst, &info))
        }
    }

    impl Drop for Decoder {
        fn drop(&mut self) {
            // SAFETY: mirrors the crate's own teardown: uninitialise (if we got that far), then destroy.
            unsafe {
                if self.initialized {
                    if let Some(uninitialize) = self.vtable().Uninitialize {
                        uninitialize(self.decoder);
                    }
                }
                self.api.WelsDestroyDecoder(self.decoder);
            }
        }
    }

    fn picture<'a>(dst: &[*mut u8; 3], info: &SBufferInfo) -> Option<Picture<'a>> {
        if info.iBufferStatus == 0 || dst.iter().any(|p| p.is_null()) {
            return None;
        }
        // SAFETY: `iBufferStatus == 1` means openh264 filled `UsrData.sSystemBuffer` and the three
        // plane pointers, which stay valid until the next decoder call — the borrow the caller holds.
        let buffer = unsafe { info.UsrData.sSystemBuffer };
        let (width, height) = (buffer.iWidth.max(0) as usize, buffer.iHeight.max(0) as usize);
        let (y_stride, uv_stride) = (buffer.iStride[0].max(0) as usize, buffer.iStride[1].max(0) as usize);
        if width == 0 || height == 0 || y_stride < width || uv_stride < width.div_ceil(2) {
            return None;
        }
        let chroma_rows = height.div_ceil(2);
        // SAFETY: plane sizes follow openh264's own I420 layout (`height * stride` luma,
        // `height / 2 * stride` chroma), the same arithmetic the `openh264` crate uses.
        let (y, u, v) = unsafe {
            (
                std::slice::from_raw_parts(dst[0], height * y_stride),
                std::slice::from_raw_parts(dst[1], chroma_rows * uv_stride),
                std::slice::from_raw_parts(dst[2], chroma_rows * uv_stride),
            )
        };
        Some(Picture { width, height, strides: (y_stride, uv_stride), y, u, v })
    }
}

/// Rewrites `max_num_ref_frames` in a sequence parameter set.
///
/// openh264 sizes its decoded-picture pool from that field (plus two) and also parks pictures
/// there while it reorders B-frames, so x264 output — every provider's — overflows the pool after a
/// few frames (`dsOutOfMemory`). Widening the declared DPB to the H.264 maximum gives the decoder
/// the room a normal decoder would size from the level limits; the extra references are never
/// indexed by the stream, so decoding is unchanged.
mod sps {
    use super::VideoError;

    /// Largest `max_num_ref_frames` the standard allows (and openh264's `MAX_REF_PIC_COUNT`).
    const MAX_REF_FRAMES: u32 = 16;

    struct BitReader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl BitReader<'_> {
        fn bit(&mut self) -> Result<u32, VideoError> {
            let byte = *self.data.get(self.pos / 8).ok_or_else(truncated)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Ok(u32::from(bit))
        }

        fn bits(&mut self, n: u32) -> Result<u32, VideoError> {
            let mut value = 0;
            for _ in 0..n {
                value = (value << 1) | self.bit()?;
            }
            Ok(value)
        }

        fn ue(&mut self) -> Result<u32, VideoError> {
            let mut zeros = 0;
            while self.bit()? == 0 {
                zeros += 1;
                if zeros > 31 {
                    return Err(VideoError::Container("malformed SPS".into()));
                }
            }
            Ok((1 << zeros) - 1 + self.bits(zeros)?)
        }

        fn se(&mut self) -> Result<i32, VideoError> {
            let k = self.ue()?;
            Ok(if k % 2 == 1 { k.div_ceil(2) as i32 } else { -((k / 2) as i32) })
        }
    }

    fn truncated() -> VideoError {
        VideoError::Container("truncated SPS".into())
    }

    fn skip_scaling_list(reader: &mut BitReader<'_>, size: u32) -> Result<(), VideoError> {
        let (mut last, mut next) = (8i32, 8i32);
        for _ in 0..size {
            if next != 0 {
                next = (last + reader.se()? + 256) % 256;
            }
            last = if next == 0 { last } else { next };
        }
        Ok(())
    }

    /// RBSP → NAL: insert `03` after any `00 00` followed by `00`..`03`.
    fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rbsp.len() + 4);
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    /// NAL → RBSP: drop emulation-prevention `03` bytes.
    fn unescape(nal: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    fn write_ue(bits: &mut Vec<bool>, value: u32) {
        let coded = value + 1;
        let len = 32 - coded.leading_zeros();
        bits.extend(std::iter::repeat_n(false, (len - 1) as usize));
        for i in (0..len).rev() {
            bits.push((coded >> i) & 1 == 1);
        }
    }

    /// The SPS NAL unit (header byte included) with `max_num_ref_frames` raised to the maximum.
    pub fn widen_dpb(nal: &[u8]) -> Result<Vec<u8>, VideoError> {
        let Some((&header, payload)) = nal.split_first() else { return Err(truncated()) };
        if header & 0x1f != 7 {
            return Err(VideoError::Container("avcC parameter set is not an SPS".into()));
        }
        let rbsp = unescape(payload);
        let mut reader = BitReader { data: &rbsp, pos: 0 };
        let profile_idc = reader.bits(8)?;
        reader.bits(8)?; // constraint flags + reserved
        reader.bits(8)?; // level_idc
        reader.ue()?; // seq_parameter_set_id
        if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
            let chroma_format_idc = reader.ue()?;
            if chroma_format_idc == 3 {
                reader.bit()?; // separate_colour_plane_flag
            }
            reader.ue()?; // bit_depth_luma_minus8
            reader.ue()?; // bit_depth_chroma_minus8
            reader.bit()?; // qpprime_y_zero_transform_bypass_flag
            if reader.bit()? == 1 {
                let lists = if chroma_format_idc != 3 { 8 } else { 12 };
                for i in 0..lists {
                    if reader.bit()? == 1 {
                        skip_scaling_list(&mut reader, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        reader.ue()?; // log2_max_frame_num_minus4
        match reader.ue()? {
            0 => {
                reader.ue()?; // log2_max_pic_order_cnt_lsb_minus4
            }
            1 => {
                reader.bit()?; // delta_pic_order_always_zero_flag
                reader.se()?; // offset_for_non_ref_pic
                reader.se()?; // offset_for_top_to_bottom_field
                let cycle = reader.ue()?; // num_ref_frames_in_pic_order_cnt_cycle
                for _ in 0..cycle {
                    reader.se()?;
                }
            }
            _ => {}
        }
        let before = reader.pos;
        let num_ref_frames = reader.ue()?;
        let after = reader.pos;
        if num_ref_frames >= MAX_REF_FRAMES {
            return Ok(nal.to_vec());
        }

        let total_bits = rbsp.len() * 8;
        let mut bits: Vec<bool> = (0..before).map(|i| (rbsp[i / 8] >> (7 - i % 8)) & 1 == 1).collect();
        write_ue(&mut bits, MAX_REF_FRAMES);
        bits.extend((after..total_bits).map(|i| (rbsp[i / 8] >> (7 - i % 8)) & 1 == 1));
        // The RBSP trailing bits (a 1 then zero padding) are part of the copied tail; re-pad to a byte.
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        let mut out = Vec::with_capacity(bits.len() / 8);
        for byte in bits.as_chunks::<8>().0 {
            out.push(byte.iter().fold(0u8, |acc, &b| (acc << 1) | u8::from(b)));
        }
        let mut patched = vec![header];
        patched.extend(escape(&out));
        Ok(patched)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Round-trips one field through the reader on a hand-built SPS-ish bit string.
        #[test]
        fn exp_golomb_round_trip() {
            for value in [0u32, 1, 2, 3, 7, 15, 16, 100] {
                let mut bits = Vec::new();
                write_ue(&mut bits, value);
                while !bits.len().is_multiple_of(8) {
                    bits.push(false);
                }
                let bytes: Vec<u8> = bits.as_chunks::<8>().0.iter().map(|b| b.iter().fold(0u8, |acc, &x| (acc << 1) | u8::from(x))).collect();
                assert_eq!(BitReader { data: &bytes, pos: 0 }.ue().unwrap(), value);
            }
        }

        #[test]
        fn escape_and_unescape_are_inverse() {
            let rbsp = [0u8, 0, 0, 0, 1, 0, 0, 3, 5, 0, 0];
            assert_eq!(unescape(&escape(&rbsp)), rbsp);
            assert_eq!(escape(&[0, 0, 0]), vec![0, 0, 3, 0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RGB: [u8; 3] = [200, 100, 50];

    fn clip_file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn assert_colour(px: &[u8], expected: [u8; 3]) {
        for (i, (got, want)) in px.iter().zip(expected).enumerate() {
            assert!((i32::from(*got) - i32::from(want)).abs() <= 8, "channel {i}: {got} vs {want} in {px:?}");
        }
    }

    #[test]
    fn probe_reports_dimensions_duration_and_audio_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "a.mp4", &encode_solid_clip(96, 64, 3, RGB).unwrap());
        let info = probe(&path).unwrap();
        assert_eq!(info, VideoInfo { width: Some(96), height: Some(64), duration_secs: Some(3.0), has_audio: false });
    }

    #[test]
    fn probe_rejects_truncated_and_non_video_files() {
        let dir = tempfile::tempdir().unwrap();
        let clip = encode_solid_clip(64, 64, 2, RGB).unwrap();
        let png = clip_file(&dir, "a.png", &crate::images::solid_png(8, 8, RGB));
        assert!(matches!(probe(&png), Err(VideoError::Container(_))), "{:?}", probe(&png).err());
        // Cut inside the `mdat`, before the trailing `moov`: no track table to read.
        let truncated = clip_file(&dir, "t.mp4", &clip[..clip.len() / 2]);
        assert!(probe(&truncated).is_err());
        let missing = dir.path().join("missing.mp4");
        assert!(matches!(probe(&missing), Err(VideoError::Io(_))));
    }

    #[test]
    fn open_reports_unsupported_codec_fourcc() {
        let dir = tempfile::tempdir().unwrap();
        let mut clip = encode_solid_clip(64, 64, 1, RGB).unwrap();
        // The sample entry's fourcc lives inside `stsd`, after the `mdat`; patch it to a codec we don't
        // decode. (A real `hvc1` entry carries an `hvcC` box; without one the demuxer rejects the
        // container first, so a made-up fourcc is the reliable way to hit the codec check.)
        let at = clip.windows(4).rposition(|w| w == b"avc1").unwrap();
        clip[at..at + 4].copy_from_slice(b"zvc9");
        let path = clip_file(&dir, "h.mp4", &clip);
        match Source::open(&path) {
            Err(VideoError::UnsupportedCodec { fourcc }) => assert_eq!(fourcc, "zvc9"),
            other => panic!("expected UnsupportedCodec, got {:?}", other.err()),
        }
    }

    #[test]
    fn poster_is_first_frame_colour_fitted_to_max_dim() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "big.mp4", &encode_solid_clip(1920, 1080, 1, RGB).unwrap());
        let poster = poster(&path, 400).unwrap();
        assert_eq!((poster.width(), poster.height()), (400, 225));
        assert_colour(&poster.get_pixel(200, 112).0[..3], RGB);
        assert_eq!(poster.get_pixel(200, 112).0[3], 255);
    }

    #[test]
    fn poster_does_not_upscale_small_clips() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "small.mp4", &encode_solid_clip(64, 48, 1, RGB).unwrap());
        let poster = poster(&path, 400).unwrap();
        assert_eq!((poster.width(), poster.height()), (64, 48));
    }

    #[test]
    fn poster_of_clip_shorter_than_100ms_uses_last_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "short.mp4", &encode_clip(64, 64, 1, 30, 1, RGB).unwrap());
        assert!(probe(&path).unwrap().duration_secs.unwrap() < POSTER_SECS);
        let poster = poster(&path, 400).unwrap();
        assert_colour(&poster.get_pixel(32, 32).0[..3], RGB);
    }

    #[test]
    fn frame_at_decodes_forward_and_returns_none_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "c.mp4", &encode_solid_clip(64, 64, 3, RGB).unwrap());
        let mut source = Source::open(&path).unwrap();
        assert_eq!(source.size(), (64, 64));
        assert_eq!(source.duration(), 3.0);
        assert_eq!(source.frame_interval(), Duration::from_secs(1));
        assert_eq!(source.frame_at(0.0).unwrap().unwrap().pts_secs, 0.0);
        assert!(source.frame_at(0.5).unwrap().is_none(), "same picture is still due");
        assert_eq!(source.frame_at(1.2).unwrap().unwrap().pts_secs, 1.0);
        assert_eq!(source.frame_at(2.9).unwrap().unwrap().pts_secs, 2.0);
    }

    #[test]
    fn frame_at_seeks_back_to_previous_sync_sample() {
        let dir = tempfile::tempdir().unwrap();
        // 2 fps, 8 frames, a keyframe every 4: sync samples at 0 s and 2 s.
        let path = clip_file(&dir, "gop.mp4", &encode_clip(64, 64, 8, 2, 4, RGB).unwrap());
        let mut source = Source::open(&path).unwrap();
        assert_eq!(source.demuxed.sync_points.iter().map(|(t, _)| *t).collect::<Vec<_>>(), vec![0.0, 2.0]);
        assert_eq!(source.frame_at(3.0).unwrap().unwrap().pts_secs, 3.0);
        assert_eq!(source.frame_at(0.5).unwrap().unwrap().pts_secs, 0.5);
        assert_eq!(source.frame_at(2.5).unwrap().unwrap().pts_secs, 2.5);
        assert_eq!(source.frame_at(1.0).unwrap().unwrap().pts_secs, 1.0);
    }

    #[test]
    fn frame_at_past_end_returns_last_frame_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "e.mp4", &encode_solid_clip(64, 64, 2, RGB).unwrap());
        let mut source = Source::open(&path).unwrap();
        assert_eq!(source.frame_at(10.0).unwrap().unwrap().pts_secs, 1.0);
        assert!(source.frame_at(10.0).unwrap().is_none());
        assert!(source.frame_at(11.0).unwrap().is_none());
        assert_eq!(source.frame_at(0.0).unwrap().unwrap().pts_secs, 0.0, "seeking back after the end restarts");
    }

    #[test]
    fn frames_are_bgra() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_file(&dir, "red.mp4", &encode_solid_clip(64, 64, 1, [255, 0, 0]).unwrap());
        let frame = Source::open(&path).unwrap().frame_at(0.0).unwrap().unwrap();
        assert_eq!(frame.bgra.len(), 64 * 64 * 4);
        let px = &frame.bgra[(32 * 64 + 32) * 4..][..4];
        assert!(px[0] <= 8 && px[1] <= 8 && px[2] >= 247 && px[3] == 255, "expected BGRA red, got {px:?}");
        assert_colour(&frame.to_rgba_image().get_pixel(32, 32).0[..3], [255, 0, 0]);
    }

    #[test]
    fn avcc_to_annex_b_handles_any_length_size() {
        let mut out = Vec::new();
        avcc_to_annex_b(&[0, 2, 0x65, 0xAA, 0, 1, 0x41], 2, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41]);
        assert!(matches!(avcc_to_annex_b(&[0, 0, 0, 9, 1], 4, &mut out), Err(VideoError::Container(_))));
        assert!(matches!(avcc_to_annex_b(&[0, 0], 4, &mut out), Err(VideoError::Container(_))));
    }
}
