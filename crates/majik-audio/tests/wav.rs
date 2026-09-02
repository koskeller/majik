//! Integration tests against a synthesized 2 s / 440 Hz mono WAV.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use majik_audio::{duration_secs, output_device_available, probe, Player};

const SAMPLE_RATE: u32 = 44_100;
const SECONDS: u32 = 2;

/// Write a 2-second 440 Hz sine to a fresh temp file and return its path.
fn write_test_wav() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "majik-audio-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("tone.wav");

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
    let total = SAMPLE_RATE * SECONDS;
    for i in 0..total {
        let t = i as f32 / SAMPLE_RATE as f32;
        let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.25;
        writer
            .write_sample((v * i16::MAX as f32) as i16)
            .expect("write sample");
    }
    writer.finalize().expect("finalize wav");
    path
}

#[test]
fn probe_reports_duration_rate_and_channels() {
    let path = write_test_wav();
    let info = probe(&path).expect("probe wav");
    assert!(
        (info.duration_secs - 2.0).abs() < 0.01,
        "duration {}",
        info.duration_secs
    );
    assert_eq!(info.sample_rate, SAMPLE_RATE);
    assert_eq!(info.channels, 1);

    let d = duration_secs(&path).expect("duration");
    assert!((d - 2.0).abs() < 0.01, "duration {d}");
}

#[test]
fn probe_rejects_missing_and_garbage_files() {
    assert!(probe(std::path::Path::new("/definitely/not/here.wav")).is_err());

    let dir = std::env::temp_dir();
    let path = dir.join(format!("majik-audio-garbage-{}.mp3", std::process::id()));
    std::fs::write(&path, b"this is not audio at all").expect("write garbage");
    assert!(probe(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

/// Playback test. Skips (with a note on stderr) when no output device can be
/// opened, e.g. on headless CI, instead of failing.
#[test]
fn player_open_seek_position() {
    if !output_device_available() {
        eprintln!("skipping player_open_seek_position: no audio output device");
        return;
    }

    let path = write_test_wav();
    let mut player = Player::open(&path).expect("open player");
    player.set_volume(0.0); // keep the test silent

    assert!((player.duration() - 2.0).abs() < 0.01);
    assert!(!player.is_playing());
    assert!(!player.finished());
    assert_eq!(player.position(), 0.0);

    // Seek while paused; the reported position must reflect it right away.
    player.seek(1.0);
    let pos = wait_for(|| player.position(), |p| (p - 1.0).abs() < 0.05);
    assert!((pos - 1.0).abs() < 0.05, "position after seek: {pos}");
    assert!(!player.is_playing());

    // Play briefly and make sure the position advances from the seek point.
    player.play();
    assert!(player.is_playing());
    std::thread::sleep(Duration::from_millis(300));
    let pos = player.position();
    assert!(pos > 1.1 && pos < 1.8, "position while playing: {pos}");

    player.pause();
    assert!(!player.is_playing());
    let frozen = player.position();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        (player.position() - frozen).abs() < 0.02,
        "position moved while paused"
    );

    // Seek back and run to the end; `finished` must flip.
    player.seek(1.8);
    player.play();
    let done = wait_for(|| player.finished(), |f| *f);
    assert!(done, "player did not finish");
    assert!(!player.is_playing());
    assert!(player.position() > 1.9);

    // Play after finish restarts from the top.
    player.play();
    assert!(player.is_playing());
    assert!(!player.finished());
    std::thread::sleep(Duration::from_millis(100));
    assert!(player.position() < 0.5, "did not restart from beginning");

    player.stop();
    assert!(!player.is_playing());
    assert!(!player.finished());
    assert_eq!(player.position(), 0.0);
}

/// The end of a clip is where the video player asks to loop: a seek back to zero right as the
/// sound runs out, and again once it has, must come back (the UI thread froze here) and restart.
#[test]
fn seeking_back_as_the_sound_ends_returns_and_restarts() {
    if !output_device_available() {
        eprintln!("skipping seeking_back_as_the_sound_ends_returns_and_restarts: no audio output device");
        return;
    }
    let path = write_test_wav();
    let mut player = Player::open(&path).expect("open player");
    player.set_volume(0.0);

    // Seeks landing in the last few milliseconds, while the sound is still draining or has just
    // run out; the video player follows each with `play`, as here.
    for _ in 0..5 {
        player.seek(1.98);
        player.play();
        std::thread::sleep(Duration::from_millis(10));
        player.seek(0.0);
        player.play();
        assert!(player.is_playing(), "playing again from the top");
        assert!(player.position() < 0.5, "from the top: {}", player.position());
    }
    // And once it has actually finished.
    player.seek(1.9);
    player.play();
    let done = wait_for(|| player.finished(), |f| *f);
    assert!(done, "player did not finish");
    player.seek(0.0);
    player.play();
    assert!(player.is_playing());
    let pos = wait_for(|| player.position(), |p| *p > 0.05);
    assert!(pos < 0.5, "restarted from the top: {pos}");
}

/// Poll `read` every 5 ms for up to 2 s until `ok` accepts the value.
fn wait_for<T>(mut read: impl FnMut() -> T, ok: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let v = read();
        if ok(&v) || Instant::now() > deadline {
            return v;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
