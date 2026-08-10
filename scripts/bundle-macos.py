#!/usr/bin/env python3
"""Assemble `BoaVoice.app` from a release build.

Running the bare binary works, but it is not an app: macOS gives an unbundled executable a
generic icon, no dock identity and no menu bar of its own, and it cannot be launched from
Finder or Spotlight. A bundle is a directory with a particular shape, so building one is
mostly copying — the content that matters is `Info.plist`.

For a voice app, three keys in there are doing work that nothing else can do:

* `NSMicrophoneUsageDescription` — **without it the app is killed the moment it opens the
  microphone.** Not refused, not degraded: the process is terminated by the system, with
  nothing in any log the app can write. This is the single most important line in the file.
* `NSHighResolutionCapable` — without it macOS runs the window through its 2× upscaler and
  every glyph comes out soft.
* `LSMinimumSystemVersion` — the vibrancy material the window relies on is a 10.14 API, and
  below that the app would run with a plain dark background.

Screen recording needs no key. It is granted by the user in System Settings the first time
something asks, and what asks here is `ffmpeg` — so the permission lands on whichever
process launched it, which is this bundle. There is no plist entry that pre-authorises it.

Usage:
    cargo build --release
    python3 scripts/bundle-macos.py [--install] [--open]
"""

import argparse
import os
import plistlib
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BINARY = os.path.join(ROOT, "target", "release", "boavoice")
ICNS = os.path.join(ROOT, "packaging", "BoaVoice.icns")
APP = os.path.join(ROOT, "dist", "BoaVoice.app")

BUNDLE_ID = "dev.boavoice.client"


def version():
    """Read the version out of the workspace Cargo.toml.

    A five-line parser rather than a TOML dependency: this script is meant to run on a bare
    machine, and the field is the first `version =` in the file.
    """
    with open(os.path.join(ROOT, "Cargo.toml"), encoding="utf-8") as handle:
        for line in handle:
            if line.strip().startswith("version"):
                return line.split("=", 1)[1].strip().strip('"')
    return "0.0.0"


def info_plist(app_version):
    return {
        "CFBundleName": "BoaVoice",
        "CFBundleDisplayName": "BoaVoice",
        "CFBundleIdentifier": BUNDLE_ID,
        "CFBundleVersion": app_version,
        "CFBundleShortVersionString": app_version,
        "CFBundleExecutable": "boavoice",
        "CFBundleIconFile": "BoaVoice",
        "CFBundlePackageType": "APPL",
        "CFBundleInfoDictionaryVersion": "6.0",
        # Retina. Without this the window is upscaled and the type goes soft.
        "NSHighResolutionCapable": True,
        # NSVisualEffectView's window-background material needs 10.14.
        "LSMinimumSystemVersion": "10.14",
        "LSUIElement": False,
        "NSHumanReadableCopyright": "MIT OR Apache-2.0",
        # The one that must not be missing. An app that opens an input device without it is
        # terminated by the system, silently.
        "NSMicrophoneUsageDescription":
            "BoaVoice uses your microphone for voice chat.",
        # Not required today — the app has no camera path — but present so that a future
        # video call does not fail in the same silent way on a build nobody thought to
        # update.
        "NSCameraUsageDescription":
            "BoaVoice does not use the camera.",
        # Attachments are saved and opened from folders the user picks.
        "NSDesktopFolderUsageDescription":
            "BoaVoice saves and opens attachments in folders you choose.",
        "NSDownloadsFolderUsageDescription":
            "BoaVoice saves and opens attachments in folders you choose.",
        # Screen sharing runs ffmpeg as a child process, which inherits this bundle's
        # sandbox and its permissions.
        "NSAppleEventsUsageDescription":
            "BoaVoice launches ffmpeg to capture your screen when you share it.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--install", action="store_true", help="also copy the bundle into /Applications"
    )
    parser.add_argument("--open", action="store_true", help="launch it when done")
    args = parser.parse_args()

    if not os.path.exists(BINARY):
        raise SystemExit(f"{BINARY} missing — run `cargo build --release` first")
    if not os.path.exists(ICNS):
        raise SystemExit(f"{ICNS} missing — run `python3 scripts/make-icon.py` first")

    # Rebuild from scratch: leaving an old binary or a stale plist behind is the kind of
    # thing that produces a bundle that runs yesterday's code.
    shutil.rmtree(APP, ignore_errors=True)
    macos = os.path.join(APP, "Contents", "MacOS")
    resources = os.path.join(APP, "Contents", "Resources")
    os.makedirs(macos)
    os.makedirs(resources)

    shutil.copy2(BINARY, os.path.join(macos, "boavoice"))
    os.chmod(os.path.join(macos, "boavoice"), 0o755)
    shutil.copy2(ICNS, os.path.join(resources, "BoaVoice.icns"))

    app_version = version()
    with open(os.path.join(APP, "Contents", "Info.plist"), "wb") as handle:
        plistlib.dump(info_plist(app_version), handle)

    # An ad-hoc signature. Unsigned bundles are refused outright on Apple silicon, and this
    # is enough for a locally built app; a distributed one would need a real identity and
    # notarisation.
    signed = subprocess.run(
        ["codesign", "--force", "--deep", "--sign", "-", APP],
        capture_output=True,
        text=True,
    )
    if signed.returncode != 0:
        print(f"warning: codesign failed: {signed.stderr.strip()}", file=sys.stderr)

    size = subprocess.run(["du", "-sh", APP], capture_output=True, text=True)
    print(f"→ {APP}  ({size.stdout.split()[0] if size.stdout else '?'}, v{app_version})")

    if not shutil.which("ffmpeg"):
        print(
            "note: ffmpeg is not on this machine's PATH. The app runs, and sharing a\n"
            "      screen will not work until it is installed (`brew install ffmpeg`).\n"
            "      Watching somebody else's screen needs nothing.",
            file=sys.stderr,
        )

    target = APP
    if args.install:
        target = install(APP)
    if args.open:
        subprocess.run(["open", target], check=False)


def install(bundle):
    """Copy `bundle` into /Applications, replacing any previous copy.

    Replacing means removing first rather than copying over the top: a bundle is a
    directory, and merging a new one into an old one leaves whatever the new build no longer
    ships still sitting there.

    A running copy is left alone by the filesystem but keeps executing the old binary, so
    the caller is told to quit it rather than being silently given a stale app.
    """
    destination = os.path.join("/Applications", os.path.basename(bundle))

    if not os.access("/Applications", os.W_OK):
        raise SystemExit(
            "/Applications is not writable by this user — copy "
            f"{bundle} there manually, or drag it in Finder"
        )

    running = (
        subprocess.run(["pgrep", "-x", "boavoice"], capture_output=True, text=True).returncode == 0
    )
    if running:
        print("note: BoaVoice is running; quit it to pick up this build")

    shutil.rmtree(destination, ignore_errors=True)
    shutil.copytree(bundle, destination, symlinks=True)

    # The signature covers paths, so re-sign in place at the new location. Moving a signed
    # bundle invalidates its signature, and macOS refuses to launch it.
    subprocess.run(
        ["codesign", "--force", "--deep", "--sign", "-", destination],
        capture_output=True,
        text=True,
    )
    print(f"→ installed to {destination}")
    # The microphone permission is remembered per *bundle path and signature*, so a fresh
    # install asks again. Worth saying, because "it stopped asking" and "it never asks" are
    # both diagnosed as bugs.
    print("note: macOS will ask for microphone access again after replacing the bundle")
    return destination


if __name__ == "__main__":
    sys.exit(main())
