#!/usr/bin/env bash
# Shared helpers for the release scripts. Source it, don't run it:
#
#   source "$(dirname "$0")/lib/release.sh"
#
# This file is the ONLY place in the repository that stamps MAJIK_CHANNEL. A packaging path that
# doesn't go through `stamp_channel` ships a Dev-channel app wearing the shipped app's name, so the
# stamp and the check that proves it landed live together here.

# The channel a released build carries. `config.rs` const-asserts the value, so a typo here fails
# the build rather than shipping a bundle whose Info.plist disagrees with where it writes its files.
RELEASE_CHANNEL="stable"

# `config::Channel::marker()` bakes this into the binary. Keep the two in step —
# `config::tests::every_bundle_script_greps_for_the_marker_the_binary_emits` fails if they drift.
CHANNEL_MARKER_PREFIX="majik-channel:"

repo_root() {
    git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel
}

# The workspace version, from cargo itself rather than by grepping Cargo.toml for `^version`, which
# picks up whichever key happens to come first.
crate_version() {
    cargo metadata --no-deps --format-version=1 |
        jq --raw-output '.packages[] | select(.name == "majik-app") | .version'
}

# Every `cargo build` that produces a shippable binary must run after this. Besides the channel it
# stamps the commit, which crash reports carry so a minidump can be matched to its symbols
# (`config::commit_sha`). The telemetry checksum seed (`MAJIK_TELEMETRY_SEED`) is read by the build
# straight from the environment the workflow hands the bundle step; it is a secret, so this file
# never spells it out, and a build without it ships telemetry the server may drop.
stamp_channel() {
    export MAJIK_CHANNEL="$RELEASE_CHANNEL"
    export MAJIK_COMMIT_SHA="$(git -C "$(repo_root)" rev-parse HEAD)"
    if [[ -z "${MAJIK_TELEMETRY_SEED:-}" ]]; then
        echo "note: MAJIK_TELEMETRY_SEED is unset; this build's telemetry carries no checksum." >&2
    fi
}

# Prove the binary we are about to ship was built with the stamp. Grepping the bytes rather than
# running the binary is deliberate: it works on a cross-compiled artifact (the x86_64 build happens
# on an arm64 runner), and it doesn't depend on the app having a console to print to.
require_channel_marker() { # <binary> <channel>
    local binary="$1" channel="$2"
    if ! LC_ALL=C grep -qa "${CHANNEL_MARKER_PREFIX}${channel}" "$binary"; then
        echo "FATAL: ${binary} was not built with MAJIK_CHANNEL=${channel}." >&2
        echo "       Every build that produces a shippable binary must call stamp_channel first." >&2
        exit 1
    fi
}

# Write the Breakpad symbols of an unstripped binary next to the bundle, so a crash report's
# minidump can be read (`minidump-stackwalk --symbols-path`, see docs/telemetry.md). `dump_syms` is
# Mozilla's (`cargo install dump_syms`); a machine without it skips the file with a note, since a
# local bundle is still a bundle. Run this before stripping.
write_symbols() { # <binary> <output .sym>
    local binary="$1" output="$2"
    if ! command -v dump_syms > /dev/null; then
        echo "note: dump_syms not installed; no symbols written for ${binary}." >&2
        return 0
    fi
    dump_syms "$binary" > "$output"
    echo "symbols: $output"
}

# Signing is all-or-nothing: a partial set of secrets would produce a bundle that looks signed and
# isn't. Missing secrets are a supported mode (a local build, a fork's PR), not an error.
have_macos_signing() {
    [[ -n "${MACOS_CERTIFICATE:-}" &&
       -n "${MACOS_CERTIFICATE_PASSWORD:-}" &&
       -n "${APPLE_SIGNING_IDENTITY:-}" ]]
}

have_macos_notarization() {
    [[ -n "${APPLE_NOTARIZATION_KEY:-}" &&
       -n "${APPLE_NOTARIZATION_KEY_ID:-}" &&
       -n "${APPLE_NOTARIZATION_ISSUER_ID:-}" ]]
}
