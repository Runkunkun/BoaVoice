//! The client's own copy of every attachment it has seen.
//!
//! This is the other half of the three-day storage policy, and the half that makes it
//! acceptable. The server deletes an attachment's bytes after three days so that a
//! self-hosted box does not grow forever; that would be a straightforwardly worse chat
//! app if it meant images disappearing from old conversations. It does not, because every
//! client that displayed an image kept it — here, permanently, keyed by the same SHA-256
//! the message carries.
//!
//! Three consequences of that are worth stating, because they are what the code is shaped
//! around:
//!
//! * **This directory is not a cache**, whatever it is called. After three days it holds
//!   the only copy. See [`crate::paths`] for why it is not in the platform's cache
//!   directory, which the system may empty without asking.
//! * **Bytes are verified before they are stored.** The hash is the file's name *and* the
//!   proof it is the right file. Storing unverified bytes under a hash that says otherwise
//!   would be undetectable later, once there is nothing left to compare against.
//! * **Deduplication is free.** The same screenshot posted in three channels is one file,
//!   because content addressing does not have to be told about the duplicate.

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use sha2::{Digest as _, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Whether this machine already has these bytes.
pub fn have(sha256: &str) -> bool {
    path(sha256).is_some_and(|path| path.is_file())
}

pub fn path(sha256: &str) -> Option<PathBuf> {
    crate::paths::attachment_path(sha256)
}

/// Store bytes under their hash, checking that the hash is theirs.
///
/// Written to a temporary name and renamed into place, so an interrupted write cannot
/// leave a truncated file — which content addressing would then make look permanently
/// valid, since the name says what it should be and nothing re-reads it.
pub fn store(sha256: &str, bytes: &[u8]) -> Result<()> {
    let Some(path) = path(sha256) else {
        bail!("{sha256:?} is not a SHA-256 digest");
    };

    let actual = sha256_hex(bytes);
    if actual != sha256 {
        // The one check that has to happen before anything is written. A wrong file cached
        // under a right-looking name is undetectable once the server's copy is gone.
        bail!("those bytes hash to {actual}, not to {sha256}");
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("part");
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

pub fn read(sha256: &str) -> Result<Vec<u8>> {
    let Some(path) = path(sha256) else { bail!("{sha256:?} is not a SHA-256 digest") };
    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

/// Every hash held locally.
pub fn list() -> Result<Vec<String>> {
    let root = crate::paths::attachment_dir();
    let mut found = Vec::new();
    let shards = match std::fs::read_dir(&root) {
        Ok(shards) => shards,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(err) => return Err(err).context("listing the attachment store"),
    };
    for shard in shards {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(shard.path())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip `.part` files from an interrupted write.
            if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                found.push(name);
            }
        }
    }
    Ok(found)
}

/// How much disk the store is using, for the settings screen.
pub fn total_bytes() -> u64 {
    list()
        .unwrap_or_default()
        .iter()
        .filter_map(|sha| path(sha))
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum()
}

/// Delete local copies older than `days`, by modification time. Returns how many went.
///
/// Only ever called when the user has explicitly set a retention period — the default is
/// to keep everything, because after three days these files are irreplaceable. Modification
/// time rather than the message date because the file is what is being aged out and the file
/// is all this function can see; a copy downloaded today of a year-old image is a year-old
/// image whose bytes arrived today, and keeping it another `days` is the safer error.
pub fn prune(days: u32) -> Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(days as u64 * 86_400))
        .context("that retention period is not a time")?;

    let mut removed = 0;
    for sha in list()? {
        let Some(path) = path(&sha) else { continue };
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if modified < cutoff {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(err) => log::warn!("cache: removing {sha}: {err}"),
            }
        }
    }
    Ok(removed)
}

/// Delete everything. For the settings screen's "forget all attachments".
pub fn clear() -> Result<usize> {
    let mut removed = 0;
    for sha in list()? {
        let Some(path) = path(&sha) else { continue };
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test with sections rather than several: the store's location is process-wide and
    /// claimed once, so two tests each wanting their own directory would fight over it.
    #[test]
    fn the_store_verifies_deduplicates_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        crate::paths::use_data_dir_for_tests(dir.path().to_path_buf());

        let bytes = b"pretend this is a png";
        let sha = sha256_hex(bytes);

        assert!(!have(&sha));
        store(&sha, bytes).unwrap();
        assert!(have(&sha));
        assert_eq!(read(&sha).unwrap(), bytes);
        assert_eq!(list().unwrap(), vec![sha.clone()]);
        assert_eq!(total_bytes(), bytes.len() as u64);

        // Storing the same content again is one file, not two.
        store(&sha, bytes).unwrap();
        assert_eq!(list().unwrap().len(), 1);

        // The check that matters: bytes that do not hash to the name they were given must
        // not be written at all, because nothing later could detect it.
        let wrong = store(&sha, b"different bytes entirely").unwrap_err();
        assert!(wrong.to_string().contains("hash to"), "{wrong}");
        assert_eq!(read(&sha).unwrap(), bytes, "the good copy is untouched");

        // A name that is not a digest cannot become a path.
        assert!(store("../../escape", bytes).is_err());
        assert!(read("../../escape").is_err());
        assert!(!have("../../escape"));

        // Nothing is old enough to prune, and zero days means never.
        assert_eq!(prune(1).unwrap(), 0);
        assert_eq!(prune(0).unwrap(), 0);

        assert_eq!(clear().unwrap(), 1);
        assert!(!have(&sha));
        assert_eq!(total_bytes(), 0);
    }

    #[test]
    fn the_digest_is_the_ordinary_sha256() {
        // The same constant the server pins, so the two cannot disagree about what a
        // cache key is.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
