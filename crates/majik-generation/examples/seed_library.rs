//! Builds a library big enough to measure the app against.
//!
//! ```sh
//! cargo run --release -p majik-generation --example seed_library -- ~/majik-perf \
//!     --images 9000 --videos 800 --audio 200 --thumbnails
//! MAJIK_LIBRARY=~/majik-perf cargo run --release -p majik-app
//! ```
//!
//! `--help` lists every switch. The run is deterministic: the same `--seed` and counts rebuild the
//! same library.

use anyhow::{bail, Context, Result};
use majik_generation::seed::{launch_hint, seed_library, SeedOptions};

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mut options: Option<SeedOptions> = None;
    let mut pending: Vec<(String, String)> = Vec::new();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            flag if flag.starts_with("--") => {
                let name = flag.trim_start_matches("--").to_string();
                if name == "thumbnails" || name == "reset" {
                    pending.push((name, "1".into()));
                } else {
                    let value = arguments.next().with_context(|| format!("--{name} needs a value"))?;
                    pending.push((name, value));
                }
            }
            root if options.is_none() => options = Some(SeedOptions::at(expand(root))),
            extra => bail!("unexpected argument {extra:?}\n\n{USAGE}"),
        }
    }

    let Some(mut options) = options else {
        println!("{USAGE}");
        bail!("no library path given");
    };
    options.progress = true;
    for (name, value) in pending {
        apply(&mut options, &name, &value)?;
    }

    let report = seed_library(&options)?;
    println!("\n{}", report.describe());
    println!("{}", launch_hint(&options.root));
    Ok(())
}

fn apply(options: &mut SeedOptions, name: &str, value: &str) -> Result<()> {
    let count = |value: &str| -> Result<usize> { value.parse().with_context(|| format!("--{name} wants a number, got {value:?}")) };
    match name {
        "images" => options.images = count(value)?,
        "videos" => options.videos = count(value)?,
        "audio" => options.audio = count(value)?,
        "imports" => options.imports = count(value)?,
        "albums" => options.albums = count(value)?,
        "pool" => options.pool = count(value)?,
        "threads" => options.threads = count(value)?.max(1),
        "long-edge" => options.long_edge = count(value)? as u32,
        "days" => options.days = count(value)? as u64,
        "seed" => options.seed = value.parse().with_context(|| format!("--seed wants a number, got {value:?}"))?,
        "thumbnails" => options.thumbnails = true,
        "reset" => options.reset = true,
        other => bail!("unknown option --{other}\n\n{USAGE}"),
    }
    Ok(())
}

/// `~` is the shell's job, but the path often arrives quoted.
fn expand(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    }
}

const USAGE: &str = "\
seed_library <library path> [options]

  --images N        completed image generations (default 2000)
  --videos N        video generations (default 200)
  --audio N         audio generations (default 100)
  --imports N       imported assets no generation owns (default 100)
  --albums N        albums, one in four of them large (default 8)
  --pool N          distinct images rendered and reused; 0 renders one per row (default 64)
  --long-edge PX    512 / 1024 / 2048 / 3840 (default 1024)
  --days N          spread the creation dates over this many days (default 365)
  --threads N       worker threads (default: cores)
  --seed N          run-to-run determinism (default 1)
  --thumbnails      render the thumbnails too, instead of leaving them to the app
  --reset           delete an existing library at the path first

The path must be empty, missing, or a majik library.";
