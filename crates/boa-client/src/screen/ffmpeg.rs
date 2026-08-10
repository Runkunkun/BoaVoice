//! Finding ffmpeg.
//!
//! This is a module rather than a `Command::new("ffmpeg")` because of one thing that catches every
//! desktop application that shells out: **an app launched from the Finder does not inherit your
//! shell's `PATH`.** It gets `/usr/bin:/bin:/usr/sbin:/sbin` and nothing else — so a machine with
//! ffmpeg sitting in `/opt/homebrew/bin`, installed and working in every terminal, looks to the app
//! exactly like a machine with no ffmpeg at all. The same is true of a `.desktop` launcher on Linux
//! and of a Start-menu shortcut on Windows.
//!
//! So it is looked for in three places, in this order:
//!
//! 1. `BOA_FFMPEG`, for anyone who wants to point it somewhere specific.
//! 2. **Inside the app**, which is where a packaged build puts a copy so that nothing has to be
//!    installed at all.
//! 3. The usual places on this platform, whether or not they are on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Where ffmpeg was found, resolved once.
///
/// Once, because the answer cannot change while the program runs and finding it costs a handful of
/// `stat` calls plus, in the worst case, one process launch.
static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The ffmpeg to use, or `None` if there is none.
pub fn path() -> Option<&'static Path> {
    FOUND.get_or_init(locate).as_deref()
}

/// A command ready to run, or `None` when ffmpeg is not there.
pub fn command() -> Option<Command> {
    let mut command = Command::new(path()?);
    // A child that inherits nothing it does not need. In particular it must not inherit stdin: ffmpeg
    // reads the terminal for its interactive keys and, given a stdin it cannot use, spends the share
    // logging about it.
    command.stdin(Stdio::null());
    Some(command)
}

/// Whether there is an ffmpeg at all.
pub fn available() -> bool {
    path().is_some()
}

/// A sentence for the user when there is not.
pub fn advice() -> String {
    let looked = candidates()
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no ffmpeg found — sharing a screen needs it, watching one does not. \
         Looked in: {looked}. Install it ({}), or set BOA_FFMPEG to its path.",
        if cfg!(target_os = "macos") {
            "brew install ffmpeg"
        } else if cfg!(target_os = "windows") {
            "winget install Gyan.FFmpeg"
        } else {
            "apt install ffmpeg"
        }
    )
}

fn locate() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("BOA_FFMPEG") {
        let named = PathBuf::from(named);
        if usable(&named) {
            log::info!("ffmpeg: {} (from BOA_FFMPEG)", named.display());
            return Some(named);
        }
        log::warn!("ffmpeg: BOA_FFMPEG points at {}, which does not run", named.display());
    }

    for candidate in candidates() {
        if usable(&candidate) {
            log::info!("ffmpeg: {}", candidate.display());
            return Some(candidate);
        }
    }

    // Last: whatever `PATH` says, which in a terminal-launched build is usually the answer already and
    // in a Finder-launched one almost never is.
    let bare = PathBuf::from(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" });
    if usable(&bare) {
        log::info!("ffmpeg: found on PATH");
        return Some(bare);
    }

    log::warn!("ffmpeg: not found");
    None
}

/// Everywhere worth looking, in order.
fn candidates() -> Vec<PathBuf> {
    let mut found = Vec::new();

    // Inside the app first. A packaged build ships a copy so that a fresh machine needs nothing
    // installed, and that copy is the one whose version this code was tested against.
    if let Ok(executable) = std::env::current_exe() {
        if let Some(dir) = executable.parent() {
            // macOS: `Contents/MacOS/boavoice` → `Contents/Resources/ffmpeg`.
            found.push(dir.join("../Resources/ffmpeg"));
            // Linux and Windows: beside the binary, which is where the AppImage and the zip put it.
            found.push(dir.join(if cfg!(target_os = "windows") { "ffmpeg.exe" } else { "ffmpeg" }));
        }
    }

    if cfg!(target_os = "macos") {
        // Homebrew on Apple silicon, Homebrew on Intel, MacPorts, and the two places a hand-installed
        // binary ends up. None of these are on the `PATH` of an app launched from the Finder.
        found.extend(
            [
                "/opt/homebrew/bin/ffmpeg",
                "/usr/local/bin/ffmpeg",
                "/opt/local/bin/ffmpeg",
                "/usr/bin/ffmpeg",
            ]
            .iter()
            .map(PathBuf::from),
        );
    } else if cfg!(target_os = "windows") {
        found.extend(
            [
                r"C:\Program Files\ffmpeg\bin\ffmpeg.exe",
                r"C:\ffmpeg\bin\ffmpeg.exe",
            ]
            .iter()
            .map(PathBuf::from),
        );
    } else {
        found.extend(
            ["/usr/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/snap/bin/ffmpeg"].iter().map(PathBuf::from),
        );
    }

    found
}

/// Whether this path is an ffmpeg that runs.
///
/// Actually run it, rather than checking that the file exists and is executable. A path can exist and
/// be the wrong architecture, a broken symlink into an uninstalled Homebrew cellar, or a shell script
/// that needs an environment this process does not have — and every one of those fails later, in the
/// middle of a share, instead of here.
fn usable(path: &Path) -> bool {
    Command::new(path)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled copy has to be looked for before anything installed, or a packaged build silently
    /// uses whatever version the machine happens to have.
    #[test]
    fn the_app_looks_inside_itself_first() {
        let found = candidates();
        assert!(!found.is_empty());
        let bundled = found
            .iter()
            .position(|path| path.to_string_lossy().contains("Resources/ffmpeg") || path.parent() == std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())).as_deref());
        let installed = found.iter().position(|path| path.starts_with("/opt") || path.starts_with("/usr") || path.starts_with("C:"));
        if let (Some(bundled), Some(installed)) = (bundled, installed) {
            assert!(bundled < installed, "the bundled copy must come first: {found:?}");
        }
    }

    /// The list is the *point* of the module: an app launched from the Finder has none of these on its
    /// `PATH`, so leaving one out is the difference between working and "install ffmpeg" on a machine
    /// that has it.
    #[cfg(target_os = "macos")]
    #[test]
    fn homebrew_is_looked_for_on_both_architectures() {
        let found: Vec<String> = candidates().iter().map(|p| p.display().to_string()).collect();
        assert!(found.iter().any(|p| p == "/opt/homebrew/bin/ffmpeg"), "Apple silicon: {found:?}");
        assert!(found.iter().any(|p| p == "/usr/local/bin/ffmpeg"), "Intel: {found:?}");
    }

    #[test]
    fn something_that_is_not_a_program_is_not_usable() {
        assert!(!usable(Path::new("/definitely/not/here/ffmpeg")));
        // A directory exists and is not a program.
        assert!(!usable(Path::new("/")));
    }

    #[test]
    fn the_advice_names_what_was_looked_for() {
        let text = advice();
        assert!(text.contains("BOA_FFMPEG"), "{text}");
        // Somebody who has it installed somewhere unusual can see that it was not looked for there.
        assert!(text.contains("Looked in:"), "{text}");
    }
}
