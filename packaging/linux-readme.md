# Majik __VERSION__ for Linux

Generate images, video and audio through hosted model providers (fal / Replicate / OpenRouter).

## Install

The one-line way, which downloads this tarball and runs its `install.sh` for you:

```sh
curl -f https://trymajik.com/install.sh | sh
```

The release also ships `majik-linux-<arch>.AppImage`: one file, nothing to install. Mark it
executable and run it, from a file manager or a terminal:

```sh
chmod +x majik-linux-x86_64.AppImage
./majik-linux-x86_64.AppImage
```

It updates itself in place. For a launcher entry and an icon, tools such as Gear Lever or
AppImageLauncher register AppImages with the desktop.

This tarball is the alternative for a conventional install under `~/.local`:

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

## Updates

Majik checks for a new version every hour (Settings → About turns this off) and installs it by
replacing the AppImage, or `bin/majik`, in place, so the file and its folder must be writable by
you, as they are after `./install.sh`. A copy in `/usr/local` or a package manager's folder can't update itself;
Settings → About says so and links to the download. Restart when it says the new version is ready.

## Known limitations on Linux

- **API keys need a Secret Service.** Keys are stored through the desktop's credential store over
  D-Bus (gnome-keyring, KWallet). Without one running, keys are not persisted and Majik will ask for
  them again on each launch.
- **Copying files to the clipboard is macOS-only.** Copying an image places the bitmap; the files
  themselves are not offered, because GPUI's Wayland/X11 clipboard has no `text/uri-list` support yet.
- **Drag-out to other applications** works on Wayland and not on X11.
