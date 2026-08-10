# Build boavoice.exe for Windows, and put it next to what it needs.
#
# The lowest-priority target of the three, and the simplest: Windows has no bundle format that
# buys anything here. An .exe with its icon compiled in is a complete, double-clickable
# program; an installer would only add a step between downloading it and running it.
#
# What this does beyond `cargo build`:
#
#   * checks that the icon was built, since it is compiled into the binary
#   * copies the result to dist\ with a name that says what it is
#   * says whether ffmpeg is on the PATH, because screen *sharing* needs it and the failure
#     otherwise appears much later as a share that sends nothing
#
# Usage (PowerShell, from the repository root):
#     .\scripts\build-windows.ps1
#
# Needs the MSVC toolchain: rustup toolchain install stable-x86_64-pc-windows-msvc

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root

$icon = Join-Path $root "packaging\icon-512.png"
if (-not (Test-Path $icon)) {
    Write-Error "$icon missing - run `python3 scripts\make-icon.py` first"
}

cargo build --release --bin boavoice
if ($LASTEXITCODE -ne 0) { Write-Error "cargo build failed" }

$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$exe = Join-Path $root "target\release\boavoice.exe"
$out = Join-Path $dist "BoaVoice.exe"
Copy-Item $exe $out -Force

$size = [math]::Round((Get-Item $out).Length / 1MB, 1)
Write-Host "-> $out ($size MB)"

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    Write-Host ""
    Write-Host "note: ffmpeg is not on the PATH. Sharing a screen needs it; watching one does not."
    Write-Host "      winget install Gyan.FFmpeg"
}

Write-Host ""
Write-Host "The .exe is self-contained apart from ffmpeg. Copy it anywhere and run it."
Write-Host "Settings and saved attachments go to %APPDATA%\BoaVoice."
