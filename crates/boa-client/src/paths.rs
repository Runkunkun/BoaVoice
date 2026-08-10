//! Where the client keeps things on each platform.
//!
//! One decision here matters more than the rest, and it is the reason this is a module
//! rather than three `dirs::` calls at the point of use: **downloaded attachments do
//! not go in the cache directory.**
//!
//! They look exactly like cache — they are copies of something that came off a server,
//! keyed by content hash. But the server deletes its copy after three days, and after
//! that the client's copy is the *only* one. A cache directory is a directory the
//! operating system, a cleanup tool, or an impatient user is entitled to empty; on
//! macOS the system does purge `~/Library/Caches` under disk pressure without asking.
//! Putting the images there would mean old conversations silently losing their pictures
//! on a machine that ran low on space. So they live in the data directory, next to the
//! settings, and are only ever removed by the app's own housekeeping.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The application's name in filesystem paths.
const APP: &str = "BoaVoice";

/// Resolved once. The data directory must not change while the program runs: the attachment
/// store, the settings file and the log would end up in different places depending on when
/// each was first touched, and a `BOA_DATA_DIR` edited mid-run would silently split the
/// app's state in two.
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Settings, the attachment store, and the last-run log.
///
/// * macOS: `~/Library/Application Support/BoaVoice`
/// * Linux: `$XDG_DATA_HOME/BoaVoice`, or `~/.local/share/BoaVoice`
/// * Windows: `%APPDATA%\BoaVoice`
///
/// The fallback when the platform has no opinion is a directory in the working
/// directory rather than a panic: a client that cannot find a home directory should
/// still run, and leaving its state somewhere visible is better than losing it.
pub fn data_dir() -> PathBuf {
    DATA_DIR.get_or_init(resolve_data_dir).clone()
}

fn resolve_data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("BOA_DATA_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_dir()
        .map(|base| base.join(APP))
        .unwrap_or_else(|| PathBuf::from(APP))
}

/// Point the store somewhere else, for tests.
///
/// Setting `BOA_DATA_DIR` from a test would work exactly once and then race: every test runs on
/// its own thread in one process, and a second test changing the variable would move the first
/// one's files out from under it. This claims the `OnceLock` instead, so whichever test gets
/// there first wins and the value never changes afterwards.
#[cfg(test)]
pub fn use_data_dir_for_tests(path: PathBuf) {
    let _ = DATA_DIR.set(path);
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// The attachment store. Deliberately not a cache directory — see the module docs.
pub fn attachment_dir() -> PathBuf {
    data_dir().join("attachments")
}

/// Where a downloaded attachment with this hash lives.
///
/// Sharded by the first two hex characters, for the same reason the server does it: one
/// flat directory with tens of thousands of files in it works, and makes every listing
/// — including a curious user's — take a visible moment.
///
/// Returns `None` for anything that is not a SHA-256 digest. The hash arrives from the
/// server and is therefore not ours to trust; a string that becomes a path must never
/// be able to contain `..`.
pub fn attachment_path(sha256: &str) -> Option<PathBuf> {
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(attachment_dir().join(&sha256[..2]).join(sha256))
}

/// The record of the last session, for when the app disappears without a word.
pub fn log_path() -> PathBuf {
    data_dir().join("last-run.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_hangs_off_one_directory() {
        let base = data_dir();
        assert!(settings_path().starts_with(&base));
        assert!(attachment_dir().starts_with(&base));
        assert!(log_path().starts_with(&base));
    }

    /// The distinction the module exists for. `dirs::cache_dir` is a directory the
    /// system may empty; attachments outlive the server's copy of themselves and so
    /// must not be in it.
    #[test]
    fn attachments_are_not_in_a_cache_directory() {
        if let Some(cache) = dirs::cache_dir() {
            assert!(
                !attachment_dir().starts_with(&cache),
                "the system is allowed to empty {}, and these files are irreplaceable",
                cache.display()
            );
        }
    }

    #[test]
    fn an_attachment_path_is_sharded_and_cannot_escape() {
        let sha = "ab".to_string() + &"cd".repeat(31);
        let path = attachment_path(&sha).unwrap();
        assert!(path.ends_with(format!("ab/{sha}")), "{}", path.display());

        assert!(attachment_path("../../../etc/passwd").is_none());
        assert!(attachment_path("").is_none());
        assert!(attachment_path(&"z".repeat(64)).is_none(), "not hex");
        assert!(attachment_path(&"a".repeat(63)).is_none(), "wrong length");
    }
}
