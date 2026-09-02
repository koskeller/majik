# Majik

A desktop app for generating images, video and audio through hosted model providers — fal,
Replicate and OpenRouter. Everything you make stays in one local library you own.

- **Your key, your bill.** Majik talks to the providers directly with your own API key. There is no
  Majik account, no server in the middle, and no markup: you pay fal / Replicate / OpenRouter what
  they charge. The composer shows the estimated cost before you press generate.
- **A real library.** Every generation is a row you can retry, recreate, favourite, file into an
  album or drag straight out into another app. The media lives in a plain folder you choose; the
  index next to it is SQLite.
- **Images, video, audio and tools.** Text-to-image, image-to-image, text-to-video, image-to-video,
  speech and dialogue, plus upscaling (images and video) and background removal — one composer,
  one set of shortcuts.
- **Native.** Rust on [GPUI](https://github.com/zed-industries/zed), the UI framework Zed is built
  on, not a web view.

## Install

Download the latest build from the [Releases page](https://github.com/koskeller/majik/releases):

| Platform | File |
| --- | --- |
| macOS (Apple silicon) | `Majik-aarch64.dmg` |
| macOS (Intel) | `Majik-x86_64.dmg` |
| Windows 10/11 (x64) | `MajikSetup-x86_64.exe` |
| Linux (x86_64) | `majik-linux-x86_64.tar.gz` — unpack and run `./install.sh` |

macOS 11 or later. On Linux you need a Vulkan-capable driver (`mesa-vulkan-drivers` covers most
integrated graphics) and a Secret Service daemon — gnome-keyring or KWallet — for API keys to
persist between launches.

On first launch Majik asks for an API key and where to keep your library. Keys go into the
platform's credential store, never into the library folder or a config file.

## Build from source

Needs a stable Rust toolchain (1.98 or later). There is no ffmpeg or other system media dependency —
video decode and encode are built from vendored source.

```sh
cargo run -p majik-app          # or: cargo run --release -p majik-app
cargo test --workspace
```

On Linux, GPUI needs its usual build dependencies first:

```sh
sudo apt-get install libasound2-dev libfontconfig-dev libwayland-dev libx11-xcb-dev \
    libxkbcommon-x11-dev libssl-dev libzstd-dev libvulkan1 mesa-vulkan-drivers clang cmake
```

[CLAUDE.md](CLAUDE.md) describes the architecture and house style: the crate layout, the vocabulary
the code and tests use, and how the suites are written. Read it before your first change.

## Licence

Majik is free software under [GPL-3.0-or-later](LICENSE). You can use it, modify it and redistribute
it; a redistributed version — modified or not — has to carry the same licence and offer its source.

Contributions need a contributor licence agreement; [CONTRIBUTING.md](CONTRIBUTING.md) explains what
it grants and the relicensing promise that comes with it. [NOTICE](NOTICE) covers the third-party
components, including the H.264 patent caveat that applies to distributed binaries.
