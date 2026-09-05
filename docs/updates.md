# Updates in Majik

Majik keeps itself current the way Zed does: once an hour, and whenever you pick **Check for
Updates…** from the application menu, it asks whether a newer version has been released,
downloads it in the background, installs it beside the running one, and then tells you it is
ready. Nothing restarts on its own: **Restart to Update** appears at the bottom of the sidebar and
on **Settings → About**, and you choose when. **Settings → About → Check automatically** turns the
hourly check off; the menu item still works.

## What is sent

The check is a single request:

```
GET https://trymajik.com/api/releases/stable/latest?os=macos&arch=aarch64
```

`os` and `arch` are the operating system and CPU architecture of this build, so the server can name
the right installer; a Linux build running as an AppImage adds `&package=appimage`. There is
nothing else in it: no id, no cookie, no version of the app beyond
the `majik/<version>` user agent every request carries. Turning usage data off does not change it,
because there is nothing in it to withhold.

The two events the updater fires when usage data is on are listed in
[telemetry.md](telemetry.md): `Update Applied` on the first launch after an update, and
`Update Failed` with the stage that failed (never the error's text).

## How it installs

The download goes to a folder of its own (the system temp folder, or `updates` beside `Majik.exe`
on Windows) and is checked against the SHA-256 the server gave before anything is touched. Then:

- **macOS**: the DMG is mounted and the `Majik.app` inside it is copied over the running one with
  `rsync` (part of macOS). The next launch runs the new version; **Restart to Update** relaunches
  the app for you. Only an installed `Majik.app` can do this; a binary run from a terminal cannot.
- **Linux, AppImage**: the new AppImage is written beside the running one and renamed over it,
  so the file you downloaded is the one that stays current. The app knows it runs as an AppImage
  from the `APPIMAGE` variable the AppImage runtime sets, and asks the server for an AppImage.
- **Linux, tarball**: the tarball is unpacked and `bin/majik` is renamed over the running binary,
  which Linux allows. When the binary lives in `<prefix>/bin` beside a `<prefix>/share` that
  `install.sh` filled, the icons there are refreshed too. The folder must be writable by you; a
  copy in `/usr/local` or from a package manager can't update itself, and About says so with a
  link to the download.
- **Windows**: the installer is kept in `updates` beside `Majik.exe` and run silently
  (`/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /update=true`) when you choose Restart to Update, or
  with `/relaunch=false` added when you quit instead. It closes what is left of the app, replaces
  the files in place, and relaunches Majik unless you were quitting.

A check that fails while automatic (you are offline, say) is logged and retried an hour later; a
manual one reports the error on Settings → About. A download or check that was in flight when
the machine went to sleep starts over on wake. Download folders older than a day are removed at
launch.

## The server

`https://trymajik.com/api/releases/<channel>/latest?os=<os>&arch=<arch>` answers

```json
{"version": "0.2.0", "url": "https://github.com/koskeller/majik/releases/download/v0.2.0/Majik-aarch64.dmg", "sha256": "…"}
```

for the latest *published* GitHub release of [koskeller/majik](https://github.com/koskeller/majik)
(drafts and pre-releases don't count, which is what makes publishing the draft the moment a
release goes out), or `404 {"error": "no build for windows/aarch64"}` for a platform the release has
no installer for. `channel` is `stable` for a shipped build. `os` is Rust's `std::env::consts::OS`
(`macos`, `windows`, `linux`); `arch` is `ARCH` (`aarch64`, `x86_64`). The asset for each pair is
the one `release.yml` publishes: `Majik-<arch>.dmg`, `MajikSetup-<arch>.exe`,
`majik-linux-<arch>.tar.gz`, or `majik-linux-<arch>.AppImage` when the query carries
`package=appimage`. `sha256` is the file's line from the release's `SHA256SUMS`; the app
skips the check when the field is absent. The server reads GitHub with a token and caches the
answer for a few minutes; the bytes come from GitHub.

A release is pulled by unpublishing it (or marking it a pre-release): the next check sees the one
before it, and an app that already installed it is not downgraded.

## The install script

`curl -f https://trymajik.com/install.sh | sh` (`script/install.sh` in the repository) asks the
same feed, downloads the build it names, checks the checksum and installs it, so an install made
that way is exactly one the app would have made for itself.

## Trying it locally

Any build honours `MAJIK_UPDATE_URL=<base>`, and a dev build checks nowhere without it. Serve a
folder that holds `dev/latest` (a dev build asks for its own channel) with the JSON above
pointing at a DMG built by `script/bundle-mac`, and run the app with the variable set; Settings →
About shows each step.
