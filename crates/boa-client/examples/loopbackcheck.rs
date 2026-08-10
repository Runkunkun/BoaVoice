//! `cargo run --example loopbackcheck`
//!
//! Says whether this machine can share its own sound, and what to do if it cannot.
//!
//! It exists because "my screen share has no audio" is a question about the *operating system*, not
//! about this app. On macOS the answer is now "nothing to do": the screen capture carries the sound
//! under the permission it already asked for, and this loopback business does not apply — it stays as
//! the fallback for a Mac where ScreenCaptureKit cannot be reached. Elsewhere, a loopback device is
//! still how it is done, and on Windows it has to be installed. This prints what was found, or the
//! advice, without joining a call or starting a share.

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if boa_client::screen::native_capture() {
        println!("capture: ScreenCaptureKit — the sound comes with the picture, nothing to install");
        println!("  and this app's own audio is excluded, so nobody in the call hears themselves.");
        println!("  Check it for real with: cargo run --example sckcheck display:1");
        println!();
        println!("Below is the fallback, used only if that framework cannot be reached at all.");
        println!();
    }
    println!(
        "ffmpeg: {}",
        if boa_client::screen::ffmpeg_available() {
            "found"
        } else if boa_client::screen::native_capture() {
            "not found — which on this platform is fine"
        } else {
            "MISSING — sharing a screen needs it"
        }
    );

    match boa_client::screen::find_loopback() {
        Ok(loopback) => {
            println!("loopback: {}", loopback.label);
            println!("  ffmpeg would read it as: -f {} -i {}", loopback.format, loopback.input);
            println!();
            println!("A share with \"Include the desktop's sound\" on will send this device's audio.");
            println!("Remember that the machine's output has to actually be *routed* there — on macOS");
            println!("that usually means a Multi-Output Device so you can hear it as well.");
        }
        Err(advice) => {
            println!("loopback: none");
            println!("  {advice}");
        }
    }
}
