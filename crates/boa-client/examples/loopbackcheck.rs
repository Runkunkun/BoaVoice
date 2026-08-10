//! `cargo run --example loopbackcheck`
//!
//! Says whether this machine can share its own sound, and what to do if it cannot.
//!
//! It exists because "my screen share has no audio" is a question about the *operating system*, not
//! about this app: no desktop platform lets a program record its own output without a loopback device,
//! and on two of the three that device has to be installed. This prints what was found, or the advice,
//! without joining a call or starting a share.

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("ffmpeg: {}", if boa_client::screen::ffmpeg_available() { "found" } else { "MISSING — sharing a screen needs it" });

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
