#!/usr/bin/env sh
# Installs Majik on macOS or Linux with one line, the way Zed's install.sh does:
#
#   curl -f https://trymajik.com/install.sh | sh
#
# It asks the release feed for the latest build for this machine, downloads it, checks the
# SHA-256 the feed gives, and installs it: `Majik.app` into /Applications on macOS, the tarball
# into ~/.local on Linux through the install.sh inside it. From then on the installed app keeps
# itself current (docs/updates.md). Windows uses the installer from the Releases page.
#
# Knobs, all optional: `MAJIK_UPDATE_URL` points at another feed (the same variable the app
# honours), `PREFIX` is the Linux install root (default ~/.local), `MAJIK_APPLICATIONS_DIR` the
# macOS one (default /Applications).
#
# Everything sits inside `main`, called on the last line, so a download cut short can't run half
# a script.
set -eu

main() {
    feed="${MAJIK_UPDATE_URL:-https://trymajik.com/api/releases}"
    platform="$(uname -s)"
    arch="$(uname -m)"

    case "$platform" in
        Darwin) os="macos" ;;
        Linux) os="linux" ;;
        *)
            echo "Majik doesn't run on $platform. Windows: download the installer from https://github.com/koskeller/majik/releases/latest" >&2
            exit 1
            ;;
    esac
    case "$arch" in
        aarch64 | arm64) arch="aarch64" ;;
        x86_64 | amd64) arch="x86_64" ;;
        *)
            echo "There is no Majik build for $arch." >&2
            exit 1
            ;;
    esac

    if command -v curl > /dev/null 2>&1; then
        fetch() { command curl -fL --silent --show-error "$@"; }
    elif command -v wget > /dev/null 2>&1; then
        fetch() { wget -qO- "$@"; }
    else
        echo "This script needs curl or wget." >&2
        exit 1
    fi

    temp="$(mktemp -d "${TMPDIR:-/tmp}/majik-install-XXXXXX")"
    trap 'cleanup' EXIT
    mount="$temp/mount"

    answer="$(fetch "$feed/stable/latest?os=$os&arch=$arch")" || {
        echo "Couldn't reach the release feed at $feed." >&2
        exit 1
    }
    url="$(printf '%s' "$answer" | sed -n 's/.*"url":"\([^"]*\)".*/\1/p')"
    version="$(printf '%s' "$answer" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')"
    sha256="$(printf '%s' "$answer" | sed -n 's/.*"sha256":"\([^"]*\)".*/\1/p')"
    if [ -z "$url" ]; then
        echo "No Majik build for $os/$arch yet: $answer" >&2
        exit 1
    fi

    file="$temp/${url##*/}"
    echo "Downloading Majik ${version} for ${os}/${arch}..."
    fetch "$url" > "$file"
    verify "$file" "$sha256"

    "$os"
}

# Refuse a download whose SHA-256 isn't the one the feed named; a feed without one is taken as is.
verify() { # <file> <sha256 or empty>
    [ -n "$2" ] || return 0
    if command -v sha256sum > /dev/null 2>&1; then
        actual="$(sha256sum "$1" | cut -d' ' -f1)"
    elif command -v shasum > /dev/null 2>&1; then
        actual="$(shasum -a 256 "$1" | cut -d' ' -f1)"
    else
        echo "note: neither sha256sum nor shasum is here, so the download wasn't checked." >&2
        return 0
    fi
    if [ "$actual" != "$2" ]; then
        echo "The download's checksum doesn't match the release's; not installing it." >&2
        exit 1
    fi
}

linux() {
    tar -xzf "$file" -C "$temp"
    unpacked="$(find "$temp" -maxdepth 1 -type d -name 'majik-linux-*' | head -n 1)"
    if [ -z "$unpacked" ] || [ ! -f "$unpacked/install.sh" ]; then
        echo "The download isn't a Majik tarball." >&2
        exit 1
    fi
    # The tarball's own installer: ~/.local/bin/majik, the desktop entry and the icons.
    sh "$unpacked/install.sh"

    binary="${PREFIX:-$HOME/.local}/bin/majik"
    if command -v ldd > /dev/null 2>&1; then
        missing="$(ldd "$binary" 2> /dev/null | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
        if [ -n "$missing" ]; then
            echo
            echo "Majik needs libraries this machine doesn't have:"
            echo "$missing" | sed 's/^/    /'
            echo "On Debian or Ubuntu: sudo apt-get install libasound2 libfontconfig1 libwayland-client0 libx11-xcb1 libxkbcommon-x11-0 libvulkan1 mesa-vulkan-drivers"
        fi
    fi
    echo
    echo "Majik $version is installed. Open it from your app launcher, or run: majik"
}

macos() {
    applications="${MAJIK_APPLICATIONS_DIR:-/Applications}"
    mkdir -p "$mount"
    hdiutil attach -quiet -nobrowse -readonly -noautoopen "$file" -mountpoint "$mount"
    app="$(cd "$mount" && echo *.app)"
    if [ ! -d "$mount/$app" ]; then
        echo "The download isn't a Majik disk image." >&2
        exit 1
    fi
    if [ -d "$applications/$app" ]; then
        echo "Replacing $applications/$app"
        rm -rf "$applications/$app"
    fi
    # ditto keeps the bundle's metadata and signature intact, which cp -R doesn't promise.
    ditto "$mount/$app" "$applications/$app"
    hdiutil detach -quiet "$mount"
    echo
    echo "Majik $version is installed at $applications/$app. Open it from Launchpad, or run: open \"$applications/$app\""
}

cleanup() {
    if [ -d "${mount:-}" ] && mount | grep -q " on $mount "; then
        hdiutil detach -quiet "$mount" 2> /dev/null || true
    fi
    rm -rf "${temp:-}"
}

main "$@"
