# Majik __VERSION__ for Linux

Generate images, video and audio through hosted model providers (fal / Replicate / OpenRouter).

## Install

```sh
./install.sh          # installs into ~/.local (override with PREFIX=/usr/local)
```

Or run it in place: `./bin/majik`.

## System requirements

Majik links no third-party libraries of its own, but GPUI needs the platform's graphics and text
libraries at runtime. On Debian/Ubuntu:

```sh
sudo apt-get install libasound2 libfontconfig1 libwayland-client0 libx11-xcb1 \
    libxkbcommon-x11-0 libvulkan1 mesa-vulkan-drivers
```

A Vulkan-capable GPU driver is required; `mesa-vulkan-drivers` covers most integrated graphics.

## Where Majik keeps things

- Library (media, `library.db`, thumbnails): `~/.local/share/majik/Library`
- Preferences: `~/.config/majik/config.json`

Point the library elsewhere with `MAJIK_LIBRARY=/path/to/folder majik`, or in Settings.

## Known limitations on Linux

- **API keys need a Secret Service.** Keys are stored through the desktop's credential store over
  D-Bus (gnome-keyring, KWallet). Without one running, keys are not persisted and Majik will ask for
  them again on each launch.
- **Copying files to the clipboard is macOS-only.** Copying an image places the bitmap; the files
  themselves are not offered, because GPUI's Wayland/X11 clipboard has no `text/uri-list` support yet.
- **Drag-out to other applications** works on Wayland and not on X11.
