//! `cargo run --example sckcheck`
//!
//! Lists what ScreenCaptureKit says can be shared: the screens, and the windows.
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
    }
    #[cfg(not(target_os = "macos"))]
    println!("ScreenCaptureKit is a macOS framework; this platform uses ffmpeg.");
}
