//! `cargo run --release --example decodecheck`
//!
//! How long the *watching* side takes per picture, split into its two halves.
//!
//! Written to answer one question with numbers instead of opinion: when a share stutters on a fast
//! network, is the picture being lost on the wire or thrown away because this machine cannot keep up?
//! The receiving path is openh264 (software, no hardware decoder involved) followed by a YUV→RGBA
//! conversion and an 8 MB allocation per 1080p frame — and if those together cost more than a frame
//! interval, the queue in front of them overflows and every dropped picture *looks* exactly like packet
//! loss.
//!
//! Prints the budget alongside the measurement, because the only number that matters is how it compares
//! to 1000/fps milliseconds.

use std::time::{Duration, Instant};

use openh264::formats::YUVSource as _;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    for (width, height, fps) in [(1_280, 720, 30), (1_920, 1_080, 30), (1_920, 1_080, 60)] {
        measure(width, height, fps);
    }
}

fn measure(width: i32, height: i32, fps: u32) {
    println!("--- {width}×{height} at {fps} fps — {:.1} ms per picture available", 1_000.0 / fps as f64);

    // Real pictures from the real encoder, so the bitstream is what a share actually sends.
    let pictures = match encode(width, height, fps) {
        Ok(pictures) if !pictures.is_empty() => pictures,
        Ok(_) => {
            println!("  the encoder produced nothing — skipping");
            return;
        }
        Err(err) => {
            println!("  no encoder here: {err}");
            return;
        }
    };
    println!("  {} pictures, {} KB", pictures.len(), pictures.iter().map(Vec::len).sum::<usize>() / 1024);

    let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
        println!("  no decoder here");
        return;
    };

    let mut decode_total = Duration::ZERO;
    let mut convert_total = Duration::ZERO;
    let mut frames = 0u32;

    for picture in &pictures {
        let started = Instant::now();
        let decoded = match decoder.decode(picture) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => continue,
            Err(err) => {
                println!("  decode failed: {err}");
                return;
            }
        };
        decode_total += started.elapsed();

        // The other half, and the one nobody thinks about: a 1080p RGBA frame is 8 MB, allocated and
        // written per picture.
        let (w, h) = decoded.dimensions();
        let started = Instant::now();
        let mut rgba = vec![0u8; w * h * 4];
        decoded.write_rgba8(&mut rgba);
        convert_total += started.elapsed();
        frames += 1;
    }

    if frames == 0 {
        println!("  nothing decoded");
        return;
    }
    let decode = decode_total.as_secs_f64() * 1_000.0 / frames as f64;
    let convert = convert_total.as_secs_f64() * 1_000.0 / frames as f64;
    let budget = 1_000.0 / fps as f64;
    println!("  decode:  {decode:.2} ms per picture");
    println!("  to RGBA: {convert:.2} ms per picture");
    println!(
        "  together {:.2} ms — {:.0}% of the budget{}",
        decode + convert,
        (decode + convert) / budget * 100.0,
        if decode + convert > budget { "  ← CANNOT KEEP UP" } else { "" }
    );
}

/// A few seconds of real screen, encoded at these settings.
#[cfg(target_os = "macos")]
fn encode(width: i32, height: i32, fps: u32) -> anyhow::Result<Vec<Vec<u8>>> {
    use boa_client::screen::mac::{capture::Capture, content};

    let sources = content::sources().map_err(|why| anyhow::anyhow!("{why}"))?;
    let source = sources
        .iter()
        .find(|source| !source.window)
        .or_else(|| sources.first())
        .ok_or_else(|| anyhow::anyhow!("nothing to capture"))?;
    let target = content::target(source).ok_or_else(|| anyhow::anyhow!("not addressable"))?;

    let capture = Capture::start(target, width as u32, height as u32, fps, 16_000, false)?;
    let mut pictures = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seen_keyframe = false;
    while Instant::now() < deadline && pictures.len() < 120 {
        match capture.take() {
            Some(picture) => {
                seen_keyframe |= picture.keyframe;
                if seen_keyframe {
                    pictures.push(picture.data);
                }
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    Ok(pictures)
}

#[cfg(not(target_os = "macos"))]
fn encode(_width: i32, _height: i32, _fps: u32) -> anyhow::Result<Vec<Vec<u8>>> {
    anyhow::bail!("this check needs the macOS capture path")
}
