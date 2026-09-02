//! An MP4 whose AAC track symphonia cannot decode: the demuxer reads it, every frame fails.
//! That is what a provider clip with a channel layout the AAC-LC decoder does not handle looks
//! like, and it must be refused up front rather than played as silence with an error per packet.

use std::path::PathBuf;

use majik_audio::{ensure_decodable, output_device_available, probe, Player};

const SAMPLE_RATE: u32 = 44_100;
const FRAME_SAMPLES: u32 = 1024;
const FRAMES: u64 = 20;

/// A valid MP4 container with a stereo AAC-LC track whose frames are junk.
fn write_junk_aac_mp4() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "majik-audio-aac-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("junk.m4a");

    let brand = |b: &[u8; 4]| mp4::FourCC { value: *b };
    let mut writer = mp4::Mp4Writer::write_start(
        std::io::Cursor::new(Vec::new()),
        &mp4::Mp4Config {
            major_brand: brand(b"isom"),
            minor_version: 512,
            compatible_brands: vec![brand(b"isom"), brand(b"iso2"), brand(b"mp41")],
            timescale: 1000,
        },
    )
    .expect("start mp4");
    writer
        .add_track(&mp4::TrackConfig {
            track_type: mp4::TrackType::Audio,
            timescale: SAMPLE_RATE,
            language: "und".into(),
            media_conf: mp4::MediaConfig::AacConfig(mp4::AacConfig {
                bitrate: 128_000,
                profile: mp4::AudioObjectType::AacLowComplexity,
                freq_index: mp4::SampleFreqIndex::Freq44100,
                chan_conf: mp4::ChannelConfig::Stereo,
            }),
        })
        .expect("add track");
    for i in 0..FRAMES {
        // Not an AAC frame: a byte pattern no element syntax survives.
        let bytes: Vec<u8> = (0..300u32).map(|k| ((k * 37 + i as u32 * 11) % 251) as u8 ^ 0xA5).collect();
        writer
            .write_sample(
                1,
                &mp4::Mp4Sample {
                    start_time: i * u64::from(FRAME_SAMPLES),
                    duration: FRAME_SAMPLES,
                    rendering_offset: 0,
                    is_sync: true,
                    bytes: bytes.into(),
                },
            )
            .expect("write sample");
    }
    writer.write_end().expect("end mp4");
    let mut bytes = writer.into_writer().into_inner();
    // The mp4 crate writes the SL descriptor's `predefined` as 0 (custom), which symphonia refuses;
    // real files say 2 (MP4). Patch the one byte, inside the `esds` box: tag 0x06, a length byte,
    // then the value.
    let esds = bytes.windows(4).position(|w| w == b"esds").expect("an esds box");
    let sl = bytes[esds..].windows(3).position(|w| w[0] == 0x06 && w[1] <= 1 && w[2] == 0x00).expect("the SL descriptor");
    bytes[esds + sl + 2] = 0x02;
    std::fs::write(&path, bytes).expect("write file");
    path
}

#[test]
fn the_container_probes_but_the_track_is_refused() {
    let path = write_junk_aac_mp4();
    let info = probe(&path).expect("the container demuxes");
    assert_eq!(info.sample_rate, SAMPLE_RATE);
    assert!((info.duration_secs - FRAMES as f64 * f64::from(FRAME_SAMPLES) / f64::from(SAMPLE_RATE)).abs() < 0.01, "{}", info.duration_secs);

    let err = ensure_decodable(&path).expect_err("no frame decodes");
    assert!(err.to_string().contains("can't be decoded"), "{err:#}");
}

#[test]
fn a_player_will_not_open_a_track_that_cannot_be_decoded() {
    if !output_device_available() {
        eprintln!("no output device; skipping");
        return;
    }
    let path = write_junk_aac_mp4();
    assert!(Player::open(&path).is_err(), "the video player falls back to silence on this error");
}
