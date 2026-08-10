# BoaVoice

Voice, screens and chat on a server you host yourself. Rust, native, no web view, and nothing
behind a paywall — the bitrate of your screen share is a number in your own settings, and the
limit is your machine and your uplink.

The red sibling plays music, the blue one plays Blu-rays; this one carries conversations. Same
Catppuccin Mocha palette, same glass panels over the platform's own blur, same drawn snake —
green instead of red.

## What it does

**Voice.** Pick your input and output device, set the gain, watch the level meter. Noise
suppression (RNNoise) cleans a laptop microphone in a room with a fan. A gate with a hang time
stops sending when you are not talking, which is the only setting here that saves bandwidth.
Mute, deafen and push-to-talk. Per-person volume. Opus at 48 kHz with in-band forward error
correction, and a jitter buffer you can tune between 20 and 300 ms.

**Screen sharing, with the machine's sound.** Resolution, frame rate and bitrate are yours to
set — 1080p60 at 8 Mbit/s by default, 4K at 120 if your hardware and your link can do it. Nothing
on the server has an opinion about it. Hardware-encoded on macOS, x264 elsewhere. Watchers
subscribe individually, so a channel of ten people watching nothing costs the sharer nothing. The
desktop's audio travels beside the picture as stereo Opus at 96 kbit/s — see
[Desktop audio](#desktop-audio).

**Chat, with attachments that expire on the server and not in your client.** The server keeps an
attachment's bytes for **three days** and its description forever. After three days the blob is
deleted; every client that displayed the image still has it, permanently, and shows it from its
own store. Old conversations keep their pictures without the box growing until the disk fills.
See [Attachments](#attachments-and-the-three-day-rule).

**Direct file transfer.** Files too big to post go straight from one machine to the other over
[magic-wormhole](https://magic-wormhole.readthedocs.io/): the server relays a short code and
never sees the bytes. There is no size limit, because there is nothing to limit.

## Running a server

```sh
cargo build --release
./target/release/boa-server --name "Home" --data-dir /srv/boavoice
```

That is the whole setup. The first account to register becomes the owner, and gets a `#general`
and a `Lounge` to start with.

**Open two ports.** TCP 8787 for chat and control, **UDP 8788 for voice and screens**. The UDP
one cannot go through an HTTP reverse proxy, and forgetting it is the single most common
self-hosting mistake: chat works perfectly and nobody can hear anybody. The client says
`no voice connection` in the bar when this is what has happened, and
`cargo run --example voicecheck` says it in more detail.

```
--bind <addr>            host:port, host, or port      (default [::]:8787)
--media-port <port>      UDP for voice and video       (default 8788)
--data-dir <path>        database and attachments      (default ./boavoice-data)
--name <name>            what clients call this server
--max-upload-mb <n>      largest attachment            (default 64)
--closed-registration    only the first account may register
--wormhole-rendezvous <url>   offer clients your own transfer relays
--wormhole-transit <url>
```

Every flag is also an environment variable (`BOA_BIND`, `BOA_MEDIA_PORT`, …). Behind a reverse
proxy, terminate TLS there and point it at the control port; the media port stays direct.

The server is one binary, one SQLite file and a directory of blobs. Back up `boavoice.db` and
you have the conversations; the blobs are three days of courier work and can be lost without
anybody noticing.

### With Docker

```sh
docker run -d --name boavoice \
  -p 8787:8787/tcp -p 8788:8788/udp \
  -v /srv/boavoice:/data \
  -e BOA_NAME="Home" \
  ghcr.io/runkunkun/boavoice-server:latest
```

Or with one of the two committed compose files, which have the environment variables written out
with their explanations:

| File | What it does |
|---|---|
| `docker-compose.yml` | pulls the published image — the fast path |
| `docker-compose.build.yml` | compiles the server on the machine that will run it, no registry involved |

```sh
docker compose up -d
docker compose pull && docker compose up -d              # to update
docker compose -f docker-compose.build.yml up -d --build # or build it here
```

### With Portainer

**Stacks → Add stack → Repository.**

| Field | Value |
|---|---|
| Repository URL | `https://github.com/Runkunkun/BoaVoice` |
| Reference | `refs/heads/main` |
| Compose path | `docker-compose.build.yml` (builds here) or `docker-compose.yml` (pulls) |

Then **Deploy the stack**. With the build file, the first deploy takes a few minutes and wants about
2 GB of memory while it compiles; after that, *Pull and redeploy* updates it.

Two things to get right, and they are the only two that catch people out:

* **The UDP port has to be open in the firewall as well**, and in a cloud provider's security group
  if there is one. Portainer publishes it because the compose file says so; the host will still drop
  it otherwise. Voice and screens go over UDP 8788 and nothing else does — chat will work perfectly
  while nobody can hear anybody.
* **Close registration once your accounts exist.** Uncomment `BOA_CLOSED_REGISTRATION` and redeploy.
  The first account is always allowed to register, so you can set it from the very beginning: you
  register, and then nobody else can.

If the pulling variant reports `manifest unknown` or asks for credentials, the package is still
private — a package published by Actions starts private even for a public repository. Set it to
public under the repository's *Packages*, or add `ghcr.io` with a `read:packages` token under
Portainer's *Registries*.

The image is built for `linux/amd64` and `linux/arm64` on every release, and holds only the
server — the client is a desktop app with a GPU surface and audio devices, so there is nothing
sensible to run in a container. Everything that survives a restart is in `/data`; mount it.

Behind a reverse proxy: point it at 8787 and let it terminate TLS. **Leave 8788/udp alone and
publish it directly** — it carries voice and screens, and no HTTP proxy can carry UDP.

One thing to do once, by hand: a package published to GitHub's registry starts **private** even
when its repository is public, so the first `docker pull` from a server will ask for credentials.
Open the package under the repository's *Packages*, and set its visibility to public — or keep it
private and `docker login ghcr.io` on the server with a token that has `read:packages`.

## Getting the client

Built apps for all three platforms are on the [releases
page](https://github.com/Runkunkun/BoaVoice/releases): a zip per macOS architecture, an AppImage,
and an `.exe`.

**On macOS the first launch needs one command.** The builds are signed ad-hoc, not notarised —
that needs a paid Apple developer identity — so macOS quarantines the download and reports that
the app "is damaged and can't be opened". It is not damaged:

```sh
unzip BoaVoice-macOS-apple-silicon.zip -d /Applications
xattr -dr com.apple.quarantine /Applications/BoaVoice.app
open /Applications/BoaVoice.app
```

macOS then asks for microphone access, and for screen recording the first time you share a screen.

## Building the client

```sh
cargo run --release --bin boavoice
```

Or build a proper app, which is what you want for the dock icon and for launching from Finder:

```sh
cargo build --release
python3 scripts/bundle-macos.py --install --open   # macOS
scripts/build-appimage.sh                          # Linux, needs appimagetool
.\scripts\build-windows.ps1                        # Windows
```

`--install` copies the bundle into `/Applications`, replacing any previous copy wholesale rather
than merging into it, and re-signs it there. Quit a running copy first, or it keeps executing
the old binary. macOS asks for microphone access on the first call, and again after the bundle
is replaced — the permission is remembered per bundle path and signature.

**Sharing a screen needs `ffmpeg` on the PATH. Watching one does not.** See
[Screen sharing](#screen-sharing-and-why-ffmpeg).

Settings and saved attachments live in `~/Library/Application Support/BoaVoice` (macOS),
`~/.local/share/BoaVoice` (Linux) or `%APPDATA%\BoaVoice` (Windows).

### When something goes wrong

`<data dir>/last-run.log` holds the last session: a start line, the audio devices that were
opened, any panic with its backtrace and the name of the thread it happened on, and a line when
the app exits through its own shutdown. A log that simply stops means the process went away
without getting there — which is a different diagnosis from a panic, and there is no other way
to tell them apart in an app launched from Finder.

The thread name in a panic line is the useful part. Most of this app's work happens off the
interface thread — a capture callback, an encoder, a socket reader — and a panic on one of those
kills that thread and *leaves the window up*, so the symptom is "my microphone stopped working"
with no error anywhere.

For voice specifically:

```sh
cargo run --release --example voicecheck -- localhost:8787 ada 'your password' Lounge 15
```

It joins for real and prints, once a second, whether the relay is answering, how many packets
went out and came in, and what the microphone is picking up. Those four numbers separate the
five different causes of "I cannot hear anybody" that all look identical from the window.

## How it is put together

```
crates/
  boa-proto/    the wire protocol: control messages and media packets
  boa-server/   the self-hostable half: chat, attachments, and the relay
  boa-client/   the app
```

```
boa-client/src/
  ui/         everything drawn. Returns an Action; never performs one.
  state       what the server has told us. Only the frame loop writes to it.
  net/        the network thread. Owns the socket; shares nothing but two channels.
  audio/      capture, encode, decode, mix. Runs on the audio callbacks' own threads.
  media/      the UDP socket for voice and video.
  screen/     capture and encode a display; decode somebody else's.
  cache       attachments kept locally, permanently, because the server does not.
  theme       the palette and the roles the UI names.
  platform/   the one thing egui cannot do portably: real window vibrancy.
```

Nothing below `ui/` knows egui exists, which is what lets the network thread, the audio callbacks
and the encoder each run on their own schedule and report back through channels. Inside `ui/`,
screens only *draw* — every state change is returned as an `Action` and applied in one place
afterwards.

### Two planes, deliberately different shapes

The **control plane** is JSON over one WebSocket per client. Everything that is a fact — who
exists, what was said, who is in which voice channel — travels here, in order, over TCP, and is
worth being able to read in a log.

The **media plane** is a 16-byte plaintext header plus an AEAD-sealed payload over UDP. Voice is
only useful if it is late by very little: a retransmitted 20 ms frame arrives after the moment it
belonged to, so loss is better than delay and TCP is the wrong tool. The header stays readable
because the relay routes on it — and it is the AEAD's associated data, so it is authenticated
even though it is not hidden. Re-labelling a packet as coming from somebody else breaks it.

### The relay is deliberately stupid

It does not decode audio, mix it, transcode video or decide what quality anybody should get. A
full SFU does all of that, and every bit of it is CPU on the box you are paying for — and quality
decisions made in the middle are the thing this project exists not to have. A packet arrives, a
map lookup says who should get it, and the same bytes go out.

What it does enforce: a stream is bound to the address that registered it, a stream may only
carry the kind of media it was allocated for, and screen media only reaches subscribers.

### Attachments and the three-day rule

An attachment's bytes live on the server for three days. Its row — name, size, dimensions,
SHA-256 — lives forever.

* The client stores every attachment it displays under its content hash, **permanently**, in the
  data directory. Not the platform's cache directory: the system is entitled to empty that, and
  after three days this is the only copy.
* Bytes are verified against the hash before being stored. A wrong file cached under a
  right-looking name would be undetectable once the server's copy is gone.
* Uploads are deduplicated by content on both sides, which makes expiry subtler than "delete what
  is old": the same screenshot posted twice is one file, and the newer post's three days win.
* The chat log says, under every image, how long the server will still have it and whether this
  machine has its own copy. A download of an expired attachment answers `410 Gone`, not `404` —
  it *existed*, and saying so is different from saying it was never there.

The knob is `--max-upload-mb` on the server (default 64) and a retention setting in the client,
which defaults to keeping everything forever.

### Screen sharing, and why ffmpeg

Screen capture is the one job here with no portable Rust answer: it is ScreenCaptureKit on macOS,
X11 or PipeWire on Linux and DXGI on Windows, each with its own permission model. The crate that
wraps all three could not be used — it pulls a second, incompatible ALSA binding and collides with
the audio stack — and writing three backends by hand is a project of its own. ffmpeg already does
it on all three, and while it is there it does the scaling and the H.264 encoding with hardware
acceleration.

So **sharing needs ffmpeg installed; watching does not.** The decoder is openh264, in-process,
built from source with the crate.

Wayland is not supported for sharing (the X11 path fails under it, with the reason in the log).

### Desktop audio

Sharing the machine's own sound is harder than it sounds, and not for a technical reason: **no
desktop operating system lets a program record its own output without help.** Microphones have a
permission model; system audio mostly has nothing at all, because it was never something
applications were expected to do. So:

| | what is needed |
|---|---|
| **Linux** | nothing — PulseAudio and PipeWire expose a monitor source per output |
| **macOS** | a loopback device: `brew install blackhole-2ch`, then route the output through it (a Multi-Output Device in Audio MIDI Setup lets you hear it too) |
| **Windows** | a virtual device — `virtual-audio-capturer` from the screen-capture-recorder package, or a cable |

`cargo run --example loopbackcheck` says what this machine has and what to install if it has
nothing. A machine with no loopback device still shares its screen; the interface says why there
is no sound rather than leaving you to notice.

The audio is a **second ffmpeg process**, not a second stream in the video one. Multiplexing both
into a container and demuxing it in Rust would add a format and a demuxer to the critical path in
order to synchronise two things that are separately timestamped on the wire anyway — and this way
a machine with no loopback device still shares its picture.

## Decisions worth knowing about

**Voice packets carry a shared session key, so the relay could listen.** One key per voice
channel, generated by the server and handed to every member. That stops everyone on the network
path and does not stop the server. It is transport encryption, not end-to-end, and the honest
version is worth stating rather than glossing: a relay that could not decrypt would be strictly
better and needs per-pair key agreement. The key is replaced when a channel empties, so somebody
who left cannot decrypt the next call.

**Direct file transfers really are direct.** The server relays one small offer message and has
nothing to do with the transfer. The bytes go over a connection the two clients negotiate, keyed
by the wormhole code — and a wormhole code is single-use, so learning it from the relay does not
help: the first party to claim it wins, and that is the recipient.

**The playback path is stereo all the way through, including voice.** Voice is mono on the wire —
stereo would double the uplink to place people in a field nobody is looking at — and is widened to
both channels by the receive thread. Carrying it as mono to the device and widening at the very end
would be slightly cheaper and would leave nowhere to put a screen share's stereo audio, which is
the one stream where the channel separation *is* the content. A consequence worth knowing: the
jitter buffer is measured in samples, so the stereo change had to double it or everybody's buffer
would have silently halved.

**Loss concealment fills the buffer it is given, not one frame.** Opus's `decode(&[])` produces as
many samples as the output slice will hold. Handed a four-frame scratch buffer it invents 80 ms of
audio per lost packet, which adds delay *while stretching the gap it exists to hide*. Found by the
`voicecheck` example, whose concealment count was half its received count.

**The expected packet is not a duplicate.** The first version of the sequence-number logic computed
`seq - expected` and treated `0` as a repeat, which dropped every second frame and replaced it with
an invented one. Audio still came out and merely sounded poor. It is now a pure function
(`classify`) with tests over gaps, duplicates, reordering and the counter wrapping.

**A NAL unit does not extend to the next unit's payload.** The bytes between them are the start
code — three of them, or four — and including those in the previous unit corrupts it. The Annex-B
reader also has to remember that a unit can be split across two reads from the pipe, because a pipe
never delivers frame-aligned chunks.

**The window tint is painted once, not twice.** It was both the clear colour and every panel's
fill, which stacked two layers of a 70%-opaque colour into 93% and hid the platform's blur behind
something that merely looked like a dark theme.

**Devices are remembered by name, not by index.** Indices are assigned in enumeration order and
change the moment a headset is plugged in, so a saved index reliably selects the wrong device the
next time hardware moves — which in a voice app means your microphone silently becomes the
webcam's. A name that has disappeared falls back to the system default, and the settings screen
says which of the two is happening.

**Muting works before you join a call.** Somebody who joins muted meant to join muted, and a mute
button that only works once you are already in a conversation is missing exactly when it is wanted.

**The audio callbacks never wait for anything.** No allocation, no locks, no I/O. They move samples
through a lock-free ring buffer and everything expensive happens on a thread. A callback that
blocks on a mutex the interface holds is not a slow frame — it is an audible click in a
conversation.

**Nothing on the control plane is per-frame.** Its most frequent message is `Speaking`, at roughly
one per talk spurt. A design that announced voice activity per 20 ms frame would put a hundred
WebSocket writes a second per person on the connection that chat shares, and stall chat behind it.

**Sends are idempotent.** A client that loses the socket after sending has no way to know whether
the message landed; resending is only safe if the server can recognise the repeat, which a unique
index on `(author, nonce)` does. Nonces are random rather than counted, because a counter restarts
at zero when the app does.

## Tests

```sh
cargo test
```

227 of them: 134 in the client, 65 in the server, 28 in the protocol. The ones that earn their keep:

- **`crates/boa-server/src/relay.rs`** runs the real relay over real UDP sockets: two people in a
  call hear each other, an unregistered stream is ignored, a forged registration cannot take over
  somebody else's stream, and rubbish on the port is counted and forgotten.
- **`crates/boa-client/src/screen/recv.rs`** encodes two frames with openh264, fragments them the
  way the sender does, reassembles them the way the watcher does, and decodes them. If the packet
  format and the NAL grouping ever disagree, this is where it shows.
- **`crates/boa-client/src/audio/ring.rs`** runs a producer and a consumer on two threads and
  checks that every sample arrives exactly once and in order.
- **`crates/boa-client/src/audio/denoise.rs`** pins the RNNoise scale conversion. RNNoise works on
  16-bit-scaled floats; handing it ±1.0 samples produces silence with no error, which gets
  diagnosed as a broken microphone.
- **`crates/boa-client/src/screen/audio.rs`** parses a real avfoundation device listing and checks
  that only its *audio* half is read. The two halves are numbered independently, so a parser that
  ignores the headings shares a webcam as the desktop's sound.
- **`crates/boa-proto/src/media.rs`** proves that tampering with the *plaintext* header breaks the
  payload, and that a (key, nonce) pair cannot repeat — the one mistake in this cipher that leaks
  the authentication key rather than a single frame.
- **`crates/boa-server/src/blobs.rs`** runs the janitor over a real database and a real directory
  and checks that the bytes go and the metadata stays.

What the tests deliberately do *not* cover: capturing a screen (it needs a permission a test
cannot grant and a display a CI machine does not have) and whether anybody can actually hear the
audio. The first is what `dist/BoaVoice.app` plus a human is for; the second is what `voicecheck`
plus a second machine is for.

## The icon

`packaging/boa-source.svg` is a boa's head seen from above, as white line art on nothing.
`scripts/make-icon.py` composites it onto a green squircle and builds the `.icns` — with its own
PNG codec, so the icon rebuilds from a plain checkout with nothing but a Python interpreter.

```sh
scripts/raster-svg.sh          # needs librsvg; only after editing the SVG
python3 scripts/make-icon.py
```

## Licence

MIT OR Apache-2.0.

Opus by the Xiph.Org Foundation, H.264 decoding by [OpenH264](https://www.openh264.org/), noise
suppression derived from Xiph's RNNoise via
[nnnoiseless](https://crates.io/crates/nnnoiseless), capture and encoding by
[ffmpeg](https://ffmpeg.org/), palette by [Catppuccin](https://catppuccin.com/).
