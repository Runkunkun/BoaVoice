//! Attachment bytes on disk, and the janitor that takes them away again.
//!
//! This is the module the whole storage policy lives in, so it is worth stating the
//! policy plainly: **the server keeps an attachment's bytes for three days and its
//! description forever.** After three days the blob is deleted and the
//! `attachments` row is marked; a client that downloaded the image still shows it
//! from its own cache, and one that never did sees a placeholder of the right size
//! with the file's name. The alternative designs both fail in ways people notice —
//! deleting the row makes old conversations lose their images *and* their captions,
//! and keeping the bytes makes a self-hosted box grow until the disk fills, which is
//! the thing this project exists to avoid.
//!
//! The store is content-addressed: a blob's name is the SHA-256 of its contents. Two
//! people posting the same screenshot cost one file. That makes deletion the subtle
//! part — see [`crate::db::Db::expired_blobs`], which will not offer a hash whose
//! bytes some *newer* attachment still needs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use sha2::{Digest as _, Sha256};

use crate::db::Db;

/// How often the janitor wakes up.
///
/// Ten minutes. The expiry it enforces is three days, so the precision is
/// irrelevant and the point is to be gentle: each pass is one indexed query and,
/// almost always, no filesystem work at all.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

pub struct Blobs {
    root: PathBuf,
}

impl Blobs {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Blobs { root })
    }

    /// Where a hash's bytes live.
    ///
    /// Sharded by the first two hex characters. One flat directory with a hundred
    /// thousand files in it still *works* on every filesystem in use, and turns every
    /// `readdir` — including the operator's `ls` — into something that takes a
    /// visible moment. 256 subdirectories is enough that this never becomes a
    /// question.
    pub fn path(&self, sha256: &str) -> Result<PathBuf> {
        // The hash reaches here from the database, so it is ours — but it is also a
        // string that becomes a path, and the one thing that must never be possible
        // is `../`. Validating the shape is two lines and closes the question
        // permanently.
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{sha256:?} is not a SHA-256 digest");
        }
        Ok(self.root.join(&sha256[..2]).join(sha256))
    }

    /// Write `bytes` and return their hash. Storing the same content twice is a
    /// no-op the second time.
    pub async fn store(&self, bytes: &[u8]) -> Result<String> {
        let sha256 = sha256_hex(bytes);
        let path = self.path(&sha256)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(sha256);
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Write to a temporary name and rename into place. A reader can otherwise
        // open a file that is still being written and get a truncated image, and —
        // worse, because it is silent — the content-addressed name makes that
        // truncated file look permanently valid.
        let temp = path.with_extension("part");
        tokio::fs::write(&temp, bytes)
            .await
            .with_context(|| format!("writing {}", temp.display()))?;
        tokio::fs::rename(&temp, &path)
            .await
            .with_context(|| format!("renaming into {}", path.display()))?;
        Ok(sha256)
    }

    pub async fn read(&self, sha256: &str) -> Result<Vec<u8>> {
        let path = self.path(sha256)?;
        tokio::fs::read(&path).await.with_context(|| format!("reading {}", path.display()))
    }

    /// Whether a blob is on disk.
    ///
    /// The download path reads the row and then the file rather than asking first, so this is used by
    /// the tests — which is exactly what it is for: checking that the janitor removed something.
    #[allow(dead_code)]
    pub async fn exists(&self, sha256: &str) -> bool {
        match self.path(sha256) {
            Ok(path) => tokio::fs::try_exists(&path).await.unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Delete a blob. Already gone counts as success.
    pub async fn remove(&self, sha256: &str) -> Result<()> {
        let path = self.path(sha256)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// Every hash currently on disk.
    ///
    /// Only used by the orphan sweep, which is why it is allowed to be the one
    /// expensive operation here.
    pub fn list(&self) -> Result<Vec<String>> {
        let mut found = Vec::new();
        let shards = match std::fs::read_dir(&self.root) {
            Ok(shards) => shards,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(err) => return Err(err.into()),
        };
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // Skip half-written files from an interrupted `store`.
                if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                    found.push(name);
                }
            }
        }
        Ok(found)
    }

    /// Total bytes held, for the operator's log line at startup.
    pub fn total_bytes(&self) -> u64 {
        let mut total = 0;
        for sha in self.list().unwrap_or_default() {
            if let Ok(path) = self.path(&sha) {
                total += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            }
        }
        total
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// One janitor pass. Returns how many blobs it removed.
///
/// Split out from the loop so a test can run it against a real database and a real
/// directory without waiting ten minutes, which is the only way to actually check
/// that the bytes go and the metadata stays.
pub async fn sweep(db: &Db, blobs: &Blobs, now: boa_proto::Millis) -> Result<usize> {
    let mut removed = 0;

    for sha in db.expired_blobs(now)? {
        // The file first, the row second. If the process dies between them the blob
        // is gone and the row still says it is there, which shows up as one broken
        // image and is fixed by the next pass. The other order would leave a file
        // that nothing references and nothing will ever look for.
        if let Err(err) = blobs.remove(&sha).await {
            log::warn!("janitor: {sha}: {err}");
            continue;
        }
        let rows = db.mark_blob_deleted(&sha)?;
        log::debug!("janitor: dropped {sha} ({rows} attachment(s) now cache-only)");
        removed += 1;
    }

    // Blobs whose rows went away entirely — a deleted message, or an upload
    // interrupted between writing the file and inserting the row. Not covered by the
    // expiry query, which walks rows and so cannot see a file with none.
    let on_disk = blobs.list()?;
    for sha in db.orphaned_blobs(&on_disk)? {
        if let Err(err) = blobs.remove(&sha).await {
            log::warn!("janitor: orphan {sha}: {err}");
            continue;
        }
        log::debug!("janitor: dropped orphan {sha}");
        removed += 1;
    }

    Ok(removed)
}

/// Run the janitor until the process ends.
pub async fn janitor(db: Arc<Db>, blobs: Arc<Blobs>) {
    // One pass immediately: a server that was down for a week has a backlog, and
    // waiting ten minutes to start on it means ten minutes of serving attachments
    // that were supposed to be gone.
    loop {
        match sweep(&db, &blobs, boa_proto::now_millis()).await {
            Ok(0) => {}
            Ok(n) => log::info!("janitor: removed {n} expired attachment blob(s)"),
            Err(err) => log::error!("janitor: {err:#}"),
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

/// Guess a content type from a file name.
///
/// The client sends one, and a client's claim about a file it is uploading is not
/// something to echo back to other clients unexamined: `text/html` on an attachment
/// that a browser then opens from the server's origin is stored cross-site
/// scripting. So the extension decides, the list is short, and anything unrecognised
/// is a download rather than something to render.
pub fn content_type_for(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// An uploaded image's pixel size, if it is an image at all.
///
/// Read from the header only — `into_dimensions` does not decode the pixels — so a
/// 40-megapixel photo costs a few hundred bytes of parsing rather than 160 MB of
/// allocation. The client uses the result to reserve the right space in the chat log
/// before the bytes arrive, which is what stops images from shoving the scroll
/// position around as they load.
pub fn image_dimensions(bytes: &[u8]) -> (u32, u32) {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    // Two error types, one for sniffing the format and one for parsing the header, so
    // this is a nested match rather than an `and_then` chain.
    let Ok(reader) = reader.with_guessed_format() else { return (0, 0) };
    reader.into_dimensions().unwrap_or((0, 0))
}

/// Strip a client-supplied file name down to something safe to store and to show.
///
/// The name is metadata, never a path — blobs are named by their hash — but it does
/// reach a `Content-Disposition` header and a client's save dialogue, so directory
/// separators, control characters and leading dots all have to go.
pub fn sanitise_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\n' && *c != '\r')
        .take(200)
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.').to_string();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_proto::{ChannelKind, Millis};

    fn blobs() -> (Blobs, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Blobs::open(dir.path().join("blobs")).unwrap(), dir)
    }

    #[test]
    fn the_digest_is_the_ordinary_sha256() {
        // The empty string's SHA-256, so a mistake in the hex formatting cannot
        // agree with itself.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn storing_the_same_bytes_twice_makes_one_file() {
        let (blobs, _dir) = blobs();
        let a = blobs.store(b"hello").await.unwrap();
        let b = blobs.store(b"hello").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(blobs.list().unwrap(), vec![a.clone()]);
        assert_eq!(blobs.read(&a).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn a_hash_can_never_escape_the_blob_directory() {
        let (blobs, _dir) = blobs();
        assert!(blobs.path("../../etc/passwd").is_err());
        assert!(blobs.path("").is_err());
        assert!(blobs.path(&"z".repeat(64)).is_err(), "not hex");
        assert!(blobs.path(&"a".repeat(63)).is_err(), "wrong length");
        assert!(blobs.path(&"a".repeat(64)).is_ok());
        assert!(!blobs.exists("../../etc/passwd").await);
    }

    #[tokio::test]
    async fn removing_something_that_is_already_gone_is_fine() {
        let (blobs, _dir) = blobs();
        let sha = blobs.store(b"x").await.unwrap();
        blobs.remove(&sha).await.unwrap();
        blobs.remove(&sha).await.unwrap();
        assert!(!blobs.exists(&sha).await);
    }

    #[tokio::test]
    async fn half_written_files_are_not_mistaken_for_blobs() {
        let (blobs, dir) = blobs();
        let sha = blobs.store(b"x").await.unwrap();
        // What an interrupted `store` leaves behind.
        let part = dir.path().join("blobs").join(&sha[..2]).join("something.part");
        std::fs::write(&part, b"incomplete").unwrap();
        assert_eq!(blobs.list().unwrap(), vec![sha]);
    }

    /// The end-to-end statement of the policy, over a real database and a real
    /// directory: after three days the bytes are gone and everything needed to show
    /// the file from a client's own cache is still there.
    #[tokio::test]
    async fn the_sweep_takes_the_bytes_and_leaves_the_description() {
        let (blobs, _dir) = blobs();
        let db = Db::open_in_memory().unwrap();
        let ada = db.create_user("ada", "Ada", "hash").unwrap();
        let general = db.create_channel("general", ChannelKind::Text).unwrap();

        let sha = blobs.store(b"pretend this is a png").await.unwrap();
        let attachment = db
            .insert_attachment(ada.id, "shot.png", 21, "image/png", 640, 480, &sha)
            .unwrap();
        db.insert_message(general.id, ada.id, "look", None, &[attachment.id]).unwrap();

        // Before expiry, nothing happens.
        assert_eq!(sweep(&db, &blobs, attachment.expires_at - 1).await.unwrap(), 0);
        assert!(blobs.exists(&sha).await);

        assert_eq!(sweep(&db, &blobs, attachment.expires_at).await.unwrap(), 1);
        assert!(!blobs.exists(&sha).await, "the bytes are gone");

        let (still_known, blob_deleted) = db.attachment(attachment.id).unwrap().unwrap();
        assert!(blob_deleted);
        assert_eq!(still_known.name, "shot.png");
        assert_eq!((still_known.width, still_known.height), (640, 480));
        assert_eq!(still_known.sha256, sha, "the client's cache key survives");

        // And the message is still there with its attachment listed.
        let (history, _) = db.history(general.id, None, 10).unwrap();
        assert_eq!(history[0].attachments.len(), 1);

        // A second pass finds nothing left to do.
        assert_eq!(sweep(&db, &blobs, attachment.expires_at).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_sweep_collects_blobs_no_row_refers_to() {
        let (blobs, _dir) = blobs();
        let db = Db::open_in_memory().unwrap();
        // A file written by an upload that never got as far as inserting its row.
        let orphan = blobs.store(b"nobody's").await.unwrap();
        assert_eq!(sweep(&db, &blobs, 0 as Millis).await.unwrap(), 1);
        assert!(!blobs.exists(&orphan).await);
    }

    #[test]
    fn content_types_come_from_the_extension_not_from_the_client() {
        assert_eq!(content_type_for("a.png"), "image/png");
        assert_eq!(content_type_for("a.JPEG"), "image/jpeg");
        assert_eq!(content_type_for("a.tar.gz"), "application/octet-stream");
        // The one that matters: an uploaded page must not come back as something a
        // browser will run in the server's origin.
        assert_eq!(content_type_for("evil.html"), "application/octet-stream");
        assert_eq!(content_type_for("evil.svg"), "application/octet-stream");
        assert_eq!(content_type_for("noextension"), "application/octet-stream");
    }

    #[test]
    fn names_are_stripped_of_paths_and_header_breakers() {
        assert_eq!(sanitise_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitise_name(r"C:\Users\ada\shot.png"), "shot.png");
        assert_eq!(sanitise_name(".hidden"), "hidden");
        assert_eq!(sanitise_name(""), "attachment");
        assert_eq!(sanitise_name("   "), "attachment");
        // A quote or a newline here would let a file name forge a response header.
        assert_eq!(sanitise_name("a\"b\nc.png"), "abc.png");
        assert!(sanitise_name(&"x".repeat(500)).len() <= 200);
    }

    #[test]
    fn image_dimensions_are_read_from_the_header_and_zero_for_anything_else() {
        // A 1x1 PNG, by hand: the smallest thing that proves the reader is wired up.
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        assert_eq!(image_dimensions(PNG), (1, 1));
        assert_eq!(image_dimensions(b"not an image at all"), (0, 0));
        assert_eq!(image_dimensions(&[]), (0, 0));
    }
}
