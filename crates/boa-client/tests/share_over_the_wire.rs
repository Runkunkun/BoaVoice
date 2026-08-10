//! A whole screen share, from the window server to a decoded picture, over real UDP sockets.
//!
//! Every piece of this path has its own test already, and all of them passed while a share died after
//! a couple of frames. That is the reason this file exists: the failure was in the *seam* between the
//! sender's fragment sizes and the receiver's queue, so only a test that runs the real capture at real
//! settings through a real socket could see it. A 1080p keyframe is around a hundred datagrams; a queue
//! that held fewer than that could never assemble one, and the picture never came back.
//!
//! What this covers: ScreenCaptureKit → VideoToolbox → Annex-B → fragments → ChaCha20-Poly1305 → UDP →
//! reassembly → the decoder every watcher uses. What it cannot cover: the internet, and a relay that is
//! somebody else's machine. Those two are why this is a *floor* rather than a guarantee.
//!
//! Skipped rather than failed where the machine cannot capture — screen recording is a permission a
//! test cannot grant itself.

#![cfg(target_os = "macos")]

use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use boa_client::media::Transport;
use boa_client::screen::{Share, Watcher};
use boa_client::settings::ScreenSettings;
use boa_proto::media::{SessionKey, MAX_DATAGRAM};

/// Stand in for the server's relay: forward every datagram to one address, unchanged.
///
/// The real relay does a map lookup and a `send_to`; this does the `send_to`. Deliberately dumb, and
/// deliberately *lossless*, because the question here is whether the client's two halves agree — a test
/// that also dropped packets could not tell a bug from the network.
fn relay(to: std::net::SocketAddr) -> (std::net::SocketAddr, std::thread::JoinHandle<u64>) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("binding a relay socket");
    // A big receive buffer is the platform's job and this is a test, so instead: a short timeout and a
    // tight loop, which is enough to keep up with one share on loopback.
    socket.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let address = socket.local_addr().unwrap();

    eprintln!("relay listening on {address}, forwarding to {to}");
    let thread = std::thread::spawn(move || {
        let mut buffer = [0u8; MAX_DATAGRAM];
        let mut forwarded = 0u64;
        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline {
            match socket.recv_from(&mut buffer) {
                Ok((len, _from)) => {
                    if socket.send_to(&buffer[..len], to).is_ok() {
                        forwarded += 1;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(err) => {
                    eprintln!("relay: {err}");
                    continue;
                }
            }
        }
        forwarded
    });
    (address, thread)
}

/// One capture at a time.
///
/// `cargo test` runs tests in a thread each, and two ScreenCaptureKit streams starting at once is how
/// "Start stream failed" happens. This is cheaper than telling everybody to pass `--test-threads=1`.
static SCREEN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The source to share: a whole screen for preference, the largest window otherwise.
///
/// ScreenCaptureKit reports **no displays at all** while the display is asleep, which is the state a
/// machine is in for most of the night. A window goes through the same stream, encoder and fragmenting,
/// so it exercises everything these tests are about — but note that a sleeping display also stops
/// delivering *window* frames, so a test run at 3 a.m. legitimately finds nothing to do.
fn pick() -> Option<boa_client::screen::Source> {
    let sources = boa_client::screen::mac::content::sources().ok()?;
    sources
        .iter()
        .find(|source| !source.window)
        .or_else(|| sources.first())
        .cloned()
}

#[test]
fn a_real_share_arrives_and_decodes() {
    let _one_at_a_time = SCREEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(source) = pick() else {
        eprintln!("SKIPPED: nothing to share");
        return;
    };
    eprintln!("sharing {}", source.label);

    let key = SessionKey::random();
    const SSRC: u32 = 42;

    // The watching side first, so the relay knows where to forward to.
    let watching = Transport::open("127.0.0.1:1".parse().unwrap(), key.clone())
        .expect("binding the watcher's socket");
    // The socket binds `0.0.0.0`, so its own idea of its address is not one anything can send to. The
    // real relay does not have this problem: it learns where to answer from the datagrams it receives.
    let watcher_port = watching.local_addr().expect("a bound socket has an address").port();
    let watcher_address =
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, watcher_port));
    let (relay_address, relay_thread) = relay(watcher_address);

    // …and now a transport that actually points at the relay, for the sender.
    let sending = Transport::open(relay_address, key.clone()).expect("binding the sender's socket");

    let (mut tap, watcher) = Watcher::start(SSRC);
    let receiving = std::thread::spawn(move || {
        let mut buffer = [0u8; MAX_DATAGRAM];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut packets = 0u64;
        while Instant::now() < deadline {
            match watching.recv(&mut buffer) {
                Ok(Some((header, payload))) => {
                    packets += 1;
                    tap.feed(&header, &payload);
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("receiving: {err:#}");
                    break;
                }
            }
        }
        packets
    });

    // The settings that were in use when this broke: 1080p, 60 fps, a generous bitrate. The numbers
    // matter — the bug only appeared once pictures were big enough to need a hundred datagrams.
    let settings = ScreenSettings { max_dimension: 1_920, fps: 60, kbps: 16_000, with_audio: false };
    // Retried, because ScreenCaptureKit refuses to start against a display that has just gone to sleep
    // — "Start stream failed" — and that is a state of the machine rather than a fault in the code.
    let mut attempt = 0;
    let share = loop {
        attempt += 1;
        match Share::start(
            sending.try_clone().expect("cloning the media socket"),
            SSRC,
            &settings,
            &source,
            1_920,
            1_080,
            None,
        ) {
            Ok(share) => break share,
            Err(err) if attempt < 3 => {
                eprintln!("attempt {attempt}: {err:#} — trying again");
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(err) => {
                eprintln!("no share here after {attempt} attempts: {err:#}");
                return;
            }
        }
    };

    // **Every picture a keyframe.** A still desktop encodes to a few kilobytes a frame, which is exactly
    // the case that always worked; the failure needed pictures of a hundred datagrams each, arriving one
    // after another. Asking for a keyframe every frame produces that on demand, without needing
    // something on screen to move.
    let watched = Instant::now();
    let mut halfway = 0u64;
    while watched.elapsed() < Duration::from_secs(5) {
        share.want_keyframe();
        std::thread::sleep(Duration::from_millis(16));
        if halfway == 0 && watched.elapsed() > Duration::from_millis(2_500) {
            halfway = watcher.frames.load(Ordering::Relaxed);
        }
    }

    let sent_pictures = share.frames.load(Ordering::Relaxed);
    let sent_packets = share.packets.load(Ordering::Relaxed);
    let decoded = watcher.frames.load(Ordering::Relaxed);
    let dropped = watcher.dropped.load(Ordering::Relaxed);
    let failed = watcher.failed.load(Ordering::Relaxed);
    drop(share);

    let received_packets = receiving.join().unwrap_or(0);
    let forwarded = relay_thread.join().unwrap_or(0);

    println!(
        "sent {sent_pictures} pictures in {sent_packets} datagrams ({:.0} per picture); \
         relay forwarded {forwarded}; watcher received {received_packets}; \
         decoded {decoded} (half of them by {halfway}); dropped {dropped}; refused {failed}"
    , sent_packets as f64 / sent_pictures.max(1) as f64);

    if sent_pictures == 0 {
        // Not a failure: a display that has gone to sleep stops delivering frames for windows as well
        // as for itself, so there is genuinely nothing to measure. Run under `caffeinate -u -d -i` to
        // hold it awake.
        eprintln!("SKIPPED: the capture produced nothing — the display is probably asleep");
        return;
    }
    // Ten, not thirty: every picture here is a forced keyframe of several hundred datagrams, so the
    // encoder produces far fewer of them than the frame rate suggests. What matters is that they keep
    // coming, which the assertions below are about.
    assert!(sent_pictures > 10, "the sender produced almost nothing: {sent_pictures}");
    // The point of forcing keyframes: if the pictures were small, this test would prove nothing.
    assert!(
        sent_packets / sent_pictures.max(1) > 10,
        "the pictures were too small to exercise reassembly: {} datagrams each",
        sent_packets / sent_pictures.max(1)
    );
    assert!(decoded > 0, "nothing decoded at all");

    // **Not corrupt.** Loss is allowed here — a software decoder handed sixty 1080p keyframes a second
    // will fall behind, and dropping whole pictures is what it should do about it. What must never
    // happen is a picture the decoder *refuses*: that means the bytes were stitched together wrongly,
    // which is the failure the old arrangement produced.
    assert_eq!(failed, 0, "the decoder refused {failed} pictures — the stream is being corrupted");

    // And it must still be going at the end, not only at the start. That is the actual symptom being
    // guarded against: a share that worked for a few frames and then died for good.
    assert!(halfway > 0, "nothing had decoded halfway through");
    assert!(
        decoded > halfway,
        "the stream stopped: {halfway} pictures by halfway, {decoded} in total"
    );
}

/// **A share of a screen that is not changing must still be watchable.**
///
/// ScreenCaptureKit delivers a frame when something changes and nothing at all when it does not, so a
/// share of a still window sends no packets whatsoever — and somebody who starts watching it sees
/// "waiting for a keyframe" until something moves. A slide, a document, a paused video: all of them.
///
/// The heartbeat re-sends the last frame twice a second, which costs a few hundred bytes and lets the
/// encoder's own two-second keyframe rule apply. This test joins the share *late*, exactly as a person
/// would, and requires a picture within a few seconds without touching the screen.
#[test]
fn joining_a_still_share_late_still_gets_a_picture() {
    let _one_at_a_time = SCREEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let Some(source) = pick() else {
        eprintln!("SKIPPED: nothing to share");
        return;
    };

    let key = SessionKey::random();
    const SSRC: u32 = 77;

    let watching = Transport::open("127.0.0.1:1".parse().unwrap(), key.clone()).unwrap();
    let watcher_port = watching.local_addr().unwrap().port();
    let (relay_address, relay_thread) =
        relay(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, watcher_port)));
    let sending = Transport::open(relay_address, key.clone()).unwrap();

    // A modest share, because this test is about whether anything arrives at all rather than about load.
    let settings = ScreenSettings { max_dimension: 1_280, fps: 30, kbps: 2_000, with_audio: false };
    let share = match Share::start(sending, SSRC, &settings, &source, 1_280, 720, None) {
        Ok(share) => share,
        Err(err) => {
            eprintln!("SKIPPED: no share here: {err:#}");
            return;
        }
    };

    // Nobody is watching for the first two seconds, and nothing on screen is touched. Under the old
    // behaviour the share would have gone completely silent by now.
    std::thread::sleep(Duration::from_secs(2));
    let (mut tap, watcher) = Watcher::start(SSRC);

    let receiving = std::thread::spawn(move || {
        let mut buffer = [0u8; MAX_DATAGRAM];
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if let Ok(Some((header, payload))) = watching.recv(&mut buffer) {
                tap.feed(&header, &payload);
            }
        }
    });

    // Six seconds is three keyframe intervals: generous, because the point is that it happens at all.
    let joined = Instant::now();
    let mut first_picture = None;
    while joined.elapsed() < Duration::from_secs(6) {
        if watcher.frames.load(Ordering::Relaxed) > 0 {
            first_picture = Some(joined.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let sent = share.frames.load(Ordering::Relaxed);
    drop(share);
    let _ = receiving.join();
    let _ = relay_thread.join();

    if sent == 0 {
        eprintln!("SKIPPED: the capture produced nothing — the display is probably asleep");
        return;
    }
    println!("{sent} pictures sent while still; a watcher joining late saw one after {first_picture:?}");
    assert!(
        first_picture.is_some(),
        "a late watcher never saw a picture of a still screen — the heartbeat is not working"
    );
}
