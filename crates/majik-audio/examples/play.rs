//! Manual playback check: `cargo run -p majik-audio --example play -- <file> [seek-secs]`
//!
//! Prints the probed info, optionally seeks, plays to the end while printing
//! the position ten times a second (the cadence the app's scrubber uses).

use std::path::PathBuf;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path: PathBuf = match args.next() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: play <audio-file> [seek-secs]");
            std::process::exit(2);
        }
    };
    let seek: Option<f64> = args
        .next()
        .and_then(|s| s.to_str().map(str::to_owned))
        .and_then(|s| s.parse().ok());

    let info = majik_audio::probe(&path)?;
    println!(
        "{}: {:.2}s, {} Hz, {} ch",
        path.display(),
        info.duration_secs,
        info.sample_rate,
        info.channels
    );

    let mut player = majik_audio::Player::open(&path)?;
    if let Some(s) = seek {
        player.seek(s);
        println!(
            "seeked to {:.2}s (position now {:.2}s)",
            s,
            player.position()
        );
    }
    player.play();

    while !player.finished() {
        print!("\r{:6.2} / {:6.2}s", player.position(), player.duration());
        use std::io::Write;
        std::io::stdout().flush().ok();
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("\rdone at {:.2}s              ", player.position());
    Ok(())
}
