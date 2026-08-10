//! Making failures visible in a GUI app.
//!
//! An app launched from Finder, a `.desktop` entry or a Start-menu shortcut has nowhere
//! to print. Its standard error goes to a pipe nobody reads, so a panic — the one thing
//! that most needs reporting — is completely silent: the window vanishes and there is
//! nothing to look at afterwards. macOS only files a crash report for a *signal*, and an
//! ordinary Rust panic that unwinds out of `main` is not one, so even the system log has
//! nothing to say.
//!
//! So this writes both ends of the app's life to a file next to its settings: a line
//! when it starts, a line when it exits cleanly, and the full details of a panic if one
//! happens. The difference between "the log ends with a clean exit" and "the log ends
//! mid-session" is itself the first useful fact when something disappears.
//!
//! There is a second reason a voice app in particular needs this. Most of its work
//! happens on threads that are not the UI: an audio callback, an encoder, a socket
//! reader. A panic on any of those kills that thread and *leaves the window up*, so the
//! symptom is "my microphone stopped working" with no error anywhere. Those panics are
//! recorded here with the thread's name, which is why every thread this app spawns is
//! given one.
//!
//! The log is truncated at each launch. It is a record of the last run, not a history —
//! the interesting session is always the one that just failed.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Redirects the log, for tests.
///
/// Without this the test below would truncate and scribble over the *real* log —
/// destroying the one record that exists of whatever the user was doing when the app
/// last failed, which is the opposite of this module's purpose.
static OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

fn log_path() -> PathBuf {
    OVERRIDE.get().cloned().unwrap_or_else(crate::paths::log_path)
}

/// Serialises writes from whichever thread panicked.
static WRITER: Mutex<()> = Mutex::new(());

/// Append one line, best-effort.
///
/// Failures are swallowed on purpose: this is the reporting path, and a diagnostic that
/// can itself fail loudly is worse than one that quietly does nothing.
fn append(line: &str) {
    let _guard = WRITER.lock();
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// Start a fresh log and install the panic hook. Call once, first thing.
pub fn install() {
    let _ = std::fs::remove_file(log_path());
    append(&format!(
        "start  BoaVoice {} (pid {}) on {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::consts::OS,
    ));

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".into());

        let message = info.payload_as_str().unwrap_or("<non-string panic payload>");

        let thread = std::thread::current();
        // The thread's name is the most useful field in the whole line: it is what says
        // whether the microphone, the encoder or the interface just died.
        let name = thread.name().unwrap_or("unnamed").to_string();

        append(&format!("PANIC  in thread '{name}' at {location}: {message}"));
        // A backtrace only appears with RUST_BACKTRACE set, but capturing it
        // unconditionally is worth the milliseconds when the alternative is asking
        // somebody to reproduce a crash with an environment variable set.
        append(&format!("       backtrace:\n{}", std::backtrace::Backtrace::force_capture()));

        // Keep the default hook's stderr output for anyone running from a terminal.
        previous(info);
    }));
}

/// Record that the app is shutting down on purpose.
///
/// The point of this line is what it tells you by being *absent*: a log with no clean
/// exit means the process went away without going through the event loop's own shutdown.
pub fn record_clean_exit(reason: &str) {
    append(&format!("exit   {reason}"));
}

/// Note something worth having in the record when a session later goes wrong.
///
/// Used sparingly, and for facts rather than for progress: which audio devices were
/// opened, whether the platform backdrop installed, when a voice session started. These
/// are the things one wants to know about a session that ended badly, and none of them
/// are visible from a screenshot.
pub fn note(what: &str) {
    append(&format!("note   {what}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not several, and deliberately so: `install` truncates the log and
    /// replaces a process-global panic hook, so two tests doing that in parallel read
    /// each other's lines and fail at random.
    #[test]
    fn the_log_distinguishes_a_panic_on_a_worker_from_a_clean_exit() {
        let path = std::env::temp_dir().join(format!("boavoice-diag-{}.log", std::process::id()));
        OVERRIDE.set(path.clone()).expect("no other test sets the override");

        install();

        // A panic on a worker must be recorded, must name the thread, and must not take
        // the process with it — which is exactly the case that leaves a voice app up
        // with a dead microphone.
        let worker = std::thread::Builder::new()
            .name("boa-capture".into())
            .spawn(|| panic!("deliberate test panic"))
            .unwrap();
        assert!(worker.join().is_err(), "the panic should reach the join");

        note("audio: opened MacBook Pro Microphone");
        record_clean_exit("test");

        let log = std::fs::read_to_string(&path).expect("the log exists");
        assert!(log.starts_with("start"), "{log}");
        assert!(log.contains("PANIC"), "{log}");
        assert!(log.contains("boa-capture"), "the thread name is the useful part: {log}");
        assert!(log.contains("deliberate test panic"), "{log}");
        assert!(log.contains("backtrace"), "{log}");
        assert!(log.contains("note   audio:"), "{log}");
        // The clean-exit line comes last, which is what tells a shutdown apart from a
        // disappearance.
        assert!(log.trim_end().ends_with("exit   test"), "{log}");

        std::fs::remove_file(&path).ok();
    }
}
