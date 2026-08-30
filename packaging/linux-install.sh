#!/usr/bin/env sh
# Install Majik into ~/.local for the current user. Run it from the unpacked tarball:
#
#   ./install.sh
#
# Uninstall by deleting the four paths it prints.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"
app_id="com.app.majik"

mkdir -p "$prefix/bin" "$prefix/share/applications"
install -m 755 "$here/bin/majik" "$prefix/bin/majik"

for size in 512 256 128; do
    dir="$prefix/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    install -m 644 "$here/share/icons/hicolor/${size}x${size}/apps/${app_id}.png" "$dir/${app_id}.png"
done

# The desktop entry's bare `Exec=majik` only resolves if ~/.local/bin is on PATH, which is not a safe
# assumption for a launcher started by the desktop shell. Point it at the installed binary.
sed "s|^Exec=majik|Exec=$prefix/bin/majik|; s|^TryExec=majik|TryExec=$prefix/bin/majik|" \
    "$here/share/applications/${app_id}.desktop" > "$prefix/share/applications/${app_id}.desktop"

command -v update-desktop-database > /dev/null 2>&1 &&
    update-desktop-database "$prefix/share/applications" 2> /dev/null || true
command -v gtk-update-icon-cache > /dev/null 2>&1 &&
    gtk-update-icon-cache -qtf "$prefix/share/icons/hicolor" 2> /dev/null || true

echo "Installed Majik:"
echo "  $prefix/bin/majik"
echo "  $prefix/share/applications/${app_id}.desktop"
echo "  $prefix/share/icons/hicolor/*/apps/${app_id}.png"
echo
case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "Note: $prefix/bin is not on your PATH; add it to run \`majik\` from a terminal." ;;
esac
