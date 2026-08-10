#!/bin/sh
# Build BoaVoice-x86_64.AppImage from a release build.
#
# An AppImage rather than a .deb or a Flatpak, because it is the format that matches what
# this project is: one file, no installation, no package manager, runs on whatever the user
# already has. The trade is that the *libraries it needs at runtime* are the host's, so this
# script checks for them and says which are missing rather than producing an AppImage that
# fails on somebody else's machine with a linker error.
#
# What is bundled: the binary and the icon. What is not: libc, ALSA, X11 and the GPU
# drivers. Those come from the host by design — a GPU driver copied out of one distribution
# does not work on another, and a bundled libc does not work anywhere.
#
# Usage:
#     cargo build --release
#     scripts/build-appimage.sh
#
# Needs `appimagetool` on the PATH:
#     https://github.com/AppImage/AppImageKit/releases
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
binary="$root/target/release/boavoice"
icon="$root/packaging/icon-512.png"
appdir="$root/dist/BoaVoice.AppDir"
arch=$(uname -m)
out="$root/dist/BoaVoice-$arch.AppImage"

[ -x "$binary" ] || {
    echo "$binary missing — run \`cargo build --release\` first" >&2
    exit 1
}
[ -f "$icon" ] || {
    echo "$icon missing — run \`python3 scripts/make-icon.py\` first" >&2
    exit 1
}

# Rebuilt from scratch: an AppDir with yesterday's binary in it looks exactly like one with
# today's.
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
         "$appdir/usr/share/icons/hicolor/512x512/apps"

cp "$binary" "$appdir/usr/bin/boavoice"
chmod +x "$appdir/usr/bin/boavoice"
cp "$icon" "$appdir/usr/share/icons/hicolor/512x512/apps/boavoice.png"
# AppImage looks for the icon at the AppDir root under the name in the desktop file, as well
# as in the theme directory. Both, because different launchers read different ones.
cp "$icon" "$appdir/boavoice.png"
cp "$icon" "$appdir/.DirIcon"

cat > "$appdir/usr/share/applications/boavoice.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=BoaVoice
Comment=Voice, screens and chat on a server you host yourself
Exec=boavoice
Icon=boavoice
Categories=Network;InstantMessaging;AudioVideo;
Terminal=false
StartupWMClass=boavoice
DESKTOP
cp "$appdir/usr/share/applications/boavoice.desktop" "$appdir/boavoice.desktop"

# AppRun, rather than a symlink to the binary. The extra layer earns its place: it puts the
# AppImage's own directory on the library path (harmless here, needed the moment anything is
# bundled), and it is where a wrapper for a future Wayland or portal shim would go.
cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
export LD_LIBRARY_PATH="$HERE/usr/lib:${LD_LIBRARY_PATH:-}"
exec "$HERE/usr/bin/boavoice" "$@"
APPRUN
chmod +x "$appdir/AppRun"

# What the host has to provide. Reported now, as a list, rather than discovered one at a
# time by whoever runs it.
missing=""
for lib in libasound.so.2 libX11.so.6 libxkbcommon.so.0; do
    if ! ldconfig -p 2>/dev/null | grep -q "$lib"; then
        missing="$missing $lib"
    fi
done
if [ -n "$missing" ]; then
    echo "note: this machine is missing:$missing" >&2
    echo "      the AppImage will still build; it needs them on the machine that runs it" >&2
fi
if ! command -v ffmpeg >/dev/null; then
    echo "note: ffmpeg is not installed here. Sharing a screen needs it; watching does not." >&2
fi

command -v appimagetool >/dev/null || {
    echo "appimagetool not found — get it from" >&2
    echo "  https://github.com/AppImage/AppImageKit/releases" >&2
    echo "The AppDir is ready at $appdir if you would rather run it directly:" >&2
    echo "  $appdir/AppRun" >&2
    exit 1
}

# ARCH is read from the environment by appimagetool and is not always inferred correctly.
ARCH="$arch" appimagetool "$appdir" "$out"
echo "→ $out"
