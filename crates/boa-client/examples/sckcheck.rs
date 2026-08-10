//! `cargo run --example sckcheck [display:3 | window:1234]`
//!
//! Lists what ScreenCaptureKit says can be shared: the screens, and the windows. Given one of those
//! names, it then captures it for three seconds and reports what came out of the encoder — which is the
//! whole sending path except for the socket.
//!
//! Worth having separately from the app because it answers the question the app cannot ask on somebody
//! else's behalf: whether the screen-recording permission is granted *to this binary*. macOS grants it
//! per executable and per signature, so the terminal, the app bundle and this example are three
//! different subjects — and "the app cannot see my windows" is otherwise indistinguishable from "there
//! are no windows".

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    #[cfg(target_os = "macos")]
    {
        use boa_client::platform::{request_screen_access, ScreenAccess};

        match request_screen_access() {
            ScreenAccess::Granted => println!("screen recording: granted"),
            ScreenAccess::AskedForIt => {
                println!("screen recording: just asked for it");
                println!("  Allow it, then run this again — macOS only grants it to a process that");
                println!("  starts afterwards.");
            }
            ScreenAccess::Unknown => println!("screen recording: no answer"),
        }

        match boa_client::screen::mac::content::sources() {
            Ok(found) => {
                let (screens, windows): (Vec<_>, Vec<_>) =
                    found.iter().partition(|source| !source.window);
                println!();
                println!("screens ({}):", screens.len());
                for source in screens {
                    println!("  {:<24} {}", source.input, source.label);
                }
                println!();
                println!("windows ({}), largest first:", windows.len());
                for source in windows.iter().take(20) {
                    println!("  {:<24} {}", source.input, source.label);
                }
                if windows.len() > 20 {
                    println!("  … and {} more", windows.len() - 20);
                }
            }
            Err(why) => println!("\nnothing to share: {why}"),
        }

        // With a source named on the command line, capture it. Nothing goes on a wire: this is the
        // encoder's output measured where the sender would pick it up.
        if let Some(wanted) = std::env::args().nth(1) {
            capture(&wanted);
        } else {
            println!();
            println!("Pass one of those names to capture it for three seconds.");
        }
    }
    #[cfg(not(target_os = "macos"))]
    println!("ScreenCaptureKit is a macOS framework; this platform uses ffmpeg.");
}

#[cfg(target_os = "macos")]
fn capture(wanted: &str) {
    use std::time::{Duration, Instant};

    use boa_client::screen::mac::capture::Capture;
    use boa_client::screen::mac::content;
    use boa_client::screen::Source;

    let source = Source { input: wanted.to_string(), label: wanted.to_string(), window: false };
    let Some(target) = content::target(&source) else {
        println!("\n{wanted:?} is not a source name — try display:1 or window:1234");
        return;
    };

    println!();
    // The user's own settings, so the numbers below are the ones that matter rather than a demo's.
    let settings = boa_client::settings::Settings::load().screen;
    let (cap_w, cap_h) = (settings.max_dimension, (settings.max_dimension * 9 / 16).max(2));
    println!("settings: up to {cap_w}×{cap_h}, {} fps, {} kbit/s", settings.fps, settings.kbps);
    let mut capture =
        match Capture::start(target, cap_w, cap_h, settings.fps, settings.kbps, true) {
        Ok(capture) => capture,
        Err(err) => {
            println!("could not capture: {err:#}");
            return;
        }
    };
    println!("capturing at {}×{} for three seconds…", capture.width, capture.height);
    println!("play something to check the sound — this app's own audio is deliberately excluded.");
    let sound = capture.sound();

    let (mut pictures, mut keyframes, mut bytes) = (0u64, 0u64, 0usize);
    let (mut biggest_key, mut biggest_delta) = (0usize, 0usize);
    let (mut most_key_fragments, mut most_delta_fragments) = (0usize, 0usize);
    let (mut buffers, mut samples, mut peak) = (0u64, 0usize, 0.0f32);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        // Drained as it arrives, not at the end: the channel is deliberately shallow, so a reader that
        // waits until afterwards measures the queue's size rather than what was captured.
        if let Some(sound) = sound.as_ref() {
            while let Ok(chunk) = sound.try_recv() {
                buffers += 1;
                samples += chunk.len();
                peak = peak.max(chunk.iter().fold(0.0f32, |loudest, s| loudest.max(s.abs())));
            }
        }
        match capture.take() {
            Some(picture) => {
                pictures += 1;
                keyframes += u64::from(picture.keyframe);
                bytes += picture.data.len();
                let fragments = picture.data.len().div_ceil(boa_proto::media::MAX_VIDEO_CHUNK);
                if picture.keyframe {
                    biggest_key = biggest_key.max(picture.data.len());
                    most_key_fragments = most_key_fragments.max(fragments);
                } else {
                    biggest_delta = biggest_delta.max(picture.data.len());
                    most_delta_fragments = most_delta_fragments.max(fragments);
                }
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }

    let seconds = start.elapsed().as_secs_f64();
    println!(
        "{pictures} pictures ({keyframes} keyframes), {:.0} kbit/s, {:.1} fps",
        (bytes as f64 * 8.0 / 1000.0) / seconds,
        pictures as f64 / seconds
    );
    println!("frames delivered by the window server: {}", capture.frames());
    println!(
        "biggest keyframe: {} KB in {most_key_fragments} datagrams; \
         biggest delta: {} KB in {most_delta_fragments}",
        biggest_key / 1024,
        biggest_delta / 1024
    );

    if sound.is_some() {
        println!(
            "sound: {buffers} buffers, {} ms of stereo, peak {peak:.3}",
            samples / 2 * 1000 / 48_000
        );
        if samples == 0 {
            println!("  nothing arrived — with `capturesAudio` on, that means silence, not a fault.");
        }
    }
    if let Some(trouble) = capture.trouble() {
        println!("trouble: {trouble}");
    }
    if pictures == 0 {
        println!("nothing came out — with the permission granted, this is worth a bug report.");
    }
}
