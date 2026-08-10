//! Everything that survives a restart, in one SQLite file.
//!
//! One connection behind a mutex, and the queries run on the async runtime's
//! threads rather than on `spawn_blocking`. That is a deliberate trade and worth
//! stating, because it is normally the wrong one: a SQLite query against a local
//! file with WAL enabled takes single-digit microseconds, which is well under the
//! cost of moving the work to another thread and back. A self-hosted server for a
//! handful of people never queues on it. What *would* break the reasoning is a slow
//! query — so there are none: no query here scans a table without an index, and the
//! only unbounded one (history) is bounded by [`HISTORY_MAX`].
//!
//! Every function is synchronous and returns owned data. That is what keeps the
//! mutex guard from ever being alive across an `await`, which would deadlock the
//! moment two connections did anything at once.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, Context as _, Result};
use boa_proto::{
    now_millis, Attachment, Channel, ChannelKind, Id, Message, Millis, User, ATTACHMENT_TTL_SECS,
};
use rusqlite::{params, Connection, OptionalExtension as _};

/// The most messages one [`boa_proto::ClientMsg::History`] can ask for.
///
/// A cap rather than trusting the request, because the field is a `u16` and a
/// client asking for 65 535 messages would have the server build a JSON frame of
/// several megabytes on the control connection — the one that voice-state changes
/// share.
pub const HISTORY_MAX: u16 = 200;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if absent) the database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// An in-memory database, for tests.
    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL so a reader never blocks the writer: the history query of somebody
        // scrolling up must not stall the insert of somebody else's message.
        // NORMAL synchronous with WAL means a power cut can lose the last
        // transactions but cannot corrupt the file, which is the right trade for
        // chat — and FULL costs an fsync per message.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Without a busy timeout, two threads reaching SQLite at once give an
        // immediate SQLITE_BUSY rather than waiting. The mutex above makes that
        // nearly impossible; "nearly" is not a reason to leave the sharp edge in,
        // since WAL checkpointing can still lock briefly.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let db = Db { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    /// The mutex, unpoisoned.
    ///
    /// A panic while holding the connection poisons it, and every later query would
    /// fail — turning one bug into a server that has to be restarted. The
    /// connection is not left in a broken state by a panic (a half-finished
    /// statement is dropped and any implicit transaction rolls back), so carrying on
    /// is both safe and much better behaved.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| {
            log::error!("database mutex was poisoned by a panic; carrying on");
            poisoned.into_inner()
        })
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        // `user_version` rather than a migrations table: there is one linear history
        // and no need to record which steps ran, only how far we got.
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
            log::info!("database: created schema v1");
        }
        Ok(())
    }

    // ----------------------------------------------------------------------- //
    // Users
    // ----------------------------------------------------------------------- //

    /// Create a user. Fails if the name is taken, case-insensitively.
    pub fn create_user(&self, name: &str, display_name: &str, password_hash: &str) -> Result<User> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO users (name, display_name, password_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![name, display_name, password_hash, now_millis()],
        )
        .map_err(|err| match err {
            rusqlite::Error::SqliteFailure(e, _)
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                anyhow!("that name is taken")
            }
            other => anyhow!(other),
        })?;
        Ok(User {
            id: Id(conn.last_insert_rowid() as u64),
            name: name.to_string(),
            display_name: display_name.to_string(),
            online: false,
        })
    }

    /// Look a user up by login name, returning them and their password hash.
    pub fn user_by_name(&self, name: &str) -> Result<Option<(User, String)>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, display_name, password_hash FROM users WHERE name = ?1",
                params![name],
                |row| {
                    Ok((
                        User {
                            id: Id(row.get::<_, i64>(0)? as u64),
                            name: row.get(1)?,
                            display_name: row.get(2)?,
                            online: false,
                        },
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?)
    }

    /// One user by id.
    ///
    /// Not on any request path today — `Ready` sends the whole roster and everything after that is
    /// an update — but it is the obvious lookup and the tests use it.
    #[allow(dead_code)]
    pub fn user(&self, id: Id) -> Result<Option<User>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, display_name FROM users WHERE id = ?1",
                params![id.0 as i64],
                row_to_user,
            )
            .optional()?)
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, name, display_name FROM users ORDER BY id")?;
        let rows = stmt.query_map([], row_to_user)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_display_name(&self, id: Id, display_name: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE users SET display_name = ?1 WHERE id = ?2",
            params![display_name, id.0 as i64],
        )?;
        Ok(())
    }

    /// Whether anybody has an account yet.
    ///
    /// Used for two things, and *not* for a third. The first account may always register, even with
    /// registration otherwise closed — which is how a self-hosted box avoids needing an out-of-band
    /// setup step — and it is the one that gets the starter channels created alongside it.
    ///
    /// What it does not do is confer any authority. **There is no admin in this server.** Every
    /// account can do the same things: post, edit and delete *its own* messages, create channels,
    /// join voice, share a screen. Nothing in the protocol deletes somebody else's message, removes a
    /// channel or touches another account, so there is nothing for an administrator to be trusted
    /// with yet. The only lever an operator has is `--closed-registration`, and that is deliberate:
    /// a role system whose only power is "can create channels" is ceremony, and one that could delete
    /// other people's messages is a feature nobody asked for.
    pub fn is_empty(&self) -> Result<bool> {
        let conn = self.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count == 0)
    }

    // ----------------------------------------------------------------------- //
    // Tokens
    // ----------------------------------------------------------------------- //

    /// Store a login token. The caller generates it; this only records it.
    pub fn store_token(&self, token: &str, user: Id, agent: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO tokens (token, user_id, created_at, agent) VALUES (?1, ?2, ?3, ?4)",
            params![token, user.0 as i64, now_millis(), agent],
        )?;
        Ok(())
    }

    /// The user a token belongs to, if it is still valid.
    pub fn user_for_token(&self, token: &str) -> Result<Option<User>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT u.id, u.name, u.display_name
                   FROM tokens t JOIN users u ON u.id = t.user_id
                  WHERE t.token = ?1",
                params![token],
                row_to_user,
            )
            .optional()?)
    }

    /// Invalidate a token.
    ///
    /// The other half of `store_token`. Signing out currently just forgets the token client-side,
    /// which leaves a row that will be accepted again if somebody has a copy of it — so this is here
    /// for the sign-out endpoint that should exist, and is exercised by the tests meanwhile.
    #[allow(dead_code)]
    pub fn revoke_token(&self, token: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM tokens WHERE token = ?1", params![token])?;
        Ok(())
    }

    // ----------------------------------------------------------------------- //
    // Channels
    // ----------------------------------------------------------------------- //

    pub fn create_channel(&self, name: &str, kind: ChannelKind) -> Result<Channel> {
        let conn = self.lock();
        // New channels go to the end of their kind's run. Computed here rather than
        // left at zero so the sidebar order is stable without the client sorting by
        // id and pretending that is a position.
        let position: i64 = conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM channels WHERE kind = ?1",
            params![kind_str(kind)],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO channels (name, kind, position, topic, created_at)
             VALUES (?1, ?2, ?3, '', ?4)",
            params![name, kind_str(kind), position, now_millis()],
        )?;
        Ok(Channel {
            id: Id(conn.last_insert_rowid() as u64),
            name: name.to_string(),
            kind,
            position: position as i32,
            topic: String::new(),
        })
    }

    pub fn channels(&self) -> Result<Vec<Channel>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, position, topic FROM channels ORDER BY kind, position, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Channel {
                id: Id(row.get::<_, i64>(0)? as u64),
                name: row.get(1)?,
                kind: parse_kind(&row.get::<_, String>(2)?),
                position: row.get(3)?,
                topic: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn channel(&self, id: Id) -> Result<Option<Channel>> {
        Ok(self.channels()?.into_iter().find(|c| c.id == id))
    }

    // ----------------------------------------------------------------------- //
    // Messages
    // ----------------------------------------------------------------------- //

    /// Insert a message, or return the existing one with the same nonce.
    ///
    /// The duplicate check is what makes sending idempotent across a reconnect. A
    /// client that sent a message and lost the socket before the acknowledgement has
    /// no way to know whether it landed; the only choices are to drop it or to resend
    /// it, and resending is only safe if the server can recognise the repeat. The
    /// unique index on `(author_id, nonce)` does that, so two racing inserts cannot
    /// both win either.
    pub fn insert_message(
        &self,
        channel: Id,
        author: Id,
        content: &str,
        nonce: Option<&str>,
        attachments: &[Id],
    ) -> Result<Message> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let created_at = now_millis();

        let existing: Option<i64> = match nonce {
            Some(nonce) => tx
                .query_row(
                    "SELECT id FROM messages WHERE author_id = ?1 AND nonce = ?2",
                    params![author.0 as i64, nonce],
                    |row| row.get(0),
                )
                .optional()?,
            None => None,
        };
        if let Some(id) = existing {
            drop(tx);
            drop(conn);
            log::debug!("message: nonce {nonce:?} already posted as {id}");
            return self
                .message(Id(id as u64))?
                .ok_or_else(|| anyhow!("message vanished between lookups"));
        }

        tx.execute(
            "INSERT INTO messages (channel_id, author_id, content, created_at, nonce)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![channel.0 as i64, author.0 as i64, content, created_at, nonce],
        )?;
        let id = Id(tx.last_insert_rowid() as u64);

        // Claim the attachments. `uploader_id = author` is the authorisation check:
        // it stops one user from attaching somebody else's upload to their own
        // message, and `message_id IS NULL` stops an attachment being reused on a
        // second message to extend its life.
        let mut attached = Vec::new();
        for a in attachments {
            let changed = tx.execute(
                "UPDATE attachments SET message_id = ?1
                  WHERE id = ?2 AND uploader_id = ?3 AND message_id IS NULL",
                params![id.0 as i64, a.0 as i64, author.0 as i64],
            )?;
            if changed == 0 {
                log::warn!("message {id}: attachment {a} is not the author's to attach");
                continue;
            }
            attached.push(*a);
        }
        tx.commit()?;

        drop(conn);
        let mut message = self
            .message(id)?
            .ok_or_else(|| anyhow!("message vanished after insert"))?;
        message.nonce = nonce.map(str::to_string);
        Ok(message)
    }

    pub fn message(&self, id: Id) -> Result<Option<Message>> {
        let conn = self.lock();
        let message = conn
            .query_row(
                "SELECT id, channel_id, author_id, content, created_at, edited_at
                   FROM messages WHERE id = ?1",
                params![id.0 as i64],
                row_to_message,
            )
            .optional()?;
        let Some(mut message) = message else { return Ok(None) };
        message.attachments = attachments_for(&conn, id)?;
        Ok(Some(message))
    }

    /// A page of a channel's history, oldest first, ending just before `before`.
    ///
    /// Oldest-first in the result even though the query walks backwards, because
    /// that is the order the chat log draws in and reversing at the boundary is one
    /// place rather than every caller.
    pub fn history(
        &self,
        channel: Id,
        before: Option<Id>,
        limit: u16,
    ) -> Result<(Vec<Message>, bool)> {
        let limit = limit.clamp(1, HISTORY_MAX) as i64;
        let conn = self.lock();
        let before = before.map(|id| id.0 as i64).unwrap_or(i64::MAX);

        // One extra row, to answer "is there more?" without a second COUNT query
        // over the same index.
        let mut stmt = conn.prepare(
            "SELECT id, channel_id, author_id, content, created_at, edited_at
               FROM messages
              WHERE channel_id = ?1 AND id < ?2
              ORDER BY id DESC
              LIMIT ?3",
        )?;
        let mut rows: Vec<Message> = stmt
            .query_map(params![channel.0 as i64, before, limit + 1], row_to_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let complete = rows.len() as i64 <= limit;
        rows.truncate(limit as usize);
        rows.reverse();

        for message in &mut rows {
            message.attachments = attachments_for(&conn, message.id)?;
        }
        Ok((rows, complete))
    }

    /// Edit a message. Returns `None` when it does not exist or is not `author`'s.
    pub fn edit_message(&self, id: Id, author: Id, content: &str) -> Result<Option<Message>> {
        {
            let conn = self.lock();
            let changed = conn.execute(
                "UPDATE messages SET content = ?1, edited_at = ?2
                  WHERE id = ?3 AND author_id = ?4",
                params![content, now_millis(), id.0 as i64, author.0 as i64],
            )?;
            if changed == 0 {
                return Ok(None);
            }
        }
        self.message(id)
    }

    /// Delete a message. Returns the channel it was in, for the fan-out.
    pub fn delete_message(&self, id: Id, author: Id) -> Result<Option<Id>> {
        let conn = self.lock();
        let channel: Option<i64> = conn
            .query_row(
                "SELECT channel_id FROM messages WHERE id = ?1 AND author_id = ?2",
                params![id.0 as i64, author.0 as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(channel) = channel else { return Ok(None) };

        // The attachment rows go too, but the blobs are left to the janitor: a
        // delete must not stall on filesystem work, and the janitor is already the
        // one thing that knows a blob can be shared by several attachments.
        conn.execute("DELETE FROM attachments WHERE message_id = ?1", params![id.0 as i64])?;
        conn.execute("DELETE FROM messages WHERE id = ?1", params![id.0 as i64])?;
        Ok(Some(Id(channel as u64)))
    }

    // ----------------------------------------------------------------------- //
    // Attachments
    // ----------------------------------------------------------------------- //

    /// Record an upload. The blob is already on disk under `sha256`.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_attachment(
        &self,
        uploader: Id,
        name: &str,
        size: u64,
        content_type: &str,
        width: u32,
        height: u32,
        sha256: &str,
    ) -> Result<Attachment> {
        let now = now_millis();
        let expires_at = now + (ATTACHMENT_TTL_SECS as Millis) * 1000;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO attachments
               (uploader_id, name, size, content_type, width, height, sha256, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                uploader.0 as i64,
                name,
                size as i64,
                content_type,
                width,
                height,
                sha256,
                now,
                expires_at
            ],
        )?;
        Ok(Attachment {
            id: Id(conn.last_insert_rowid() as u64),
            name: name.to_string(),
            size,
            content_type: content_type.to_string(),
            width,
            height,
            sha256: sha256.to_string(),
            expires_at,
        })
    }

    /// What a download request needs: where the bytes are and what to call them.
    pub fn attachment(&self, id: Id) -> Result<Option<(Attachment, bool)>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, size, content_type, width, height, sha256, expires_at, blob_deleted
                   FROM attachments WHERE id = ?1",
                params![id.0 as i64],
                |row| {
                    Ok((
                        Attachment {
                            id: Id(row.get::<_, i64>(0)? as u64),
                            name: row.get(1)?,
                            size: row.get::<_, i64>(2)? as u64,
                            content_type: row.get(3)?,
                            width: row.get(4)?,
                            height: row.get(5)?,
                            sha256: row.get(6)?,
                            expires_at: row.get(7)?,
                        },
                        row.get::<_, i64>(8)? != 0,
                    ))
                },
            )
            .optional()?)
    }

    /// Attachments whose time is up and whose blob is still on disk.
    ///
    /// Returns the hash rather than the row, and only hashes that *no* live
    /// attachment still needs — uploads are deduplicated by content, so the same
    /// image posted in two channels is one file, and deleting it when the older
    /// message expires would blank the newer one three days early.
    pub fn expired_blobs(&self, now: Millis) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT sha256 FROM attachments
              WHERE expires_at <= ?1 AND blob_deleted = 0
                AND sha256 NOT IN (SELECT sha256 FROM attachments WHERE expires_at > ?1)",
        )?;
        let rows = stmt.query_map(params![now], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark every attachment with this hash as no longer having a blob.
    ///
    /// The rows stay. That is the whole design: a client that downloaded the image
    /// keeps showing it from its own cache, and needs the metadata — name, size,
    /// dimensions, hash — to find it there and to draw the right-sized gap for
    /// somebody who never had it.
    pub fn mark_blob_deleted(&self, sha256: &str) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute(
            "UPDATE attachments SET blob_deleted = 1 WHERE sha256 = ?1",
            params![sha256],
        )?)
    }

    /// Hashes with no attachment row left at all, for blobs orphaned by a delete.
    pub fn orphaned_blobs(&self, known: &[String]) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut orphans = Vec::new();
        let mut stmt = conn.prepare("SELECT 1 FROM attachments WHERE sha256 = ?1 LIMIT 1")?;
        for sha in known {
            let referenced: Option<i64> = stmt.query_row(params![sha], |row| row.get(0)).optional()?;
            if referenced.is_none() {
                orphans.push(sha.clone());
            }
        }
        Ok(orphans)
    }
}

fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: Id(row.get::<_, i64>(0)? as u64),
        name: row.get(1)?,
        display_name: row.get(2)?,
        online: false,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: Id(row.get::<_, i64>(0)? as u64),
        channel: Id(row.get::<_, i64>(1)? as u64),
        author: Id(row.get::<_, i64>(2)? as u64),
        content: row.get(3)?,
        created_at: row.get(4)?,
        edited_at: row.get(5)?,
        attachments: Vec::new(),
        nonce: None,
    })
}

fn attachments_for(conn: &Connection, message: Id) -> Result<Vec<Attachment>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, size, content_type, width, height, sha256, expires_at
           FROM attachments WHERE message_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![message.0 as i64], |row| {
        Ok(Attachment {
            id: Id(row.get::<_, i64>(0)? as u64),
            name: row.get(1)?,
            size: row.get::<_, i64>(2)? as u64,
            content_type: row.get(3)?,
            width: row.get(4)?,
            height: row.get(5)?,
            sha256: row.get(6)?,
            expires_at: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn kind_str(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Text => "text",
        ChannelKind::Voice => "voice",
    }
}

/// Parse a channel kind, defaulting to text.
///
/// A row with an unrecognised kind is a database written by a newer server. Text is
/// the safe reading: the channel shows up and carries messages, rather than
/// disappearing from the sidebar with no explanation.
fn parse_kind(text: &str) -> ChannelKind {
    match text {
        "voice" => ChannelKind::Voice,
        _ => ChannelKind::Text,
    }
}

const SCHEMA_V1: &str = r#"
CREATE TABLE users (
    id            INTEGER PRIMARY KEY,
    -- NOCASE so "Ada" and "ada" cannot both exist; people expect login names to
    -- be one identity, and two accounts differing only in case is a phishing
    -- surface rather than a feature.
    name          TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    display_name  TEXT    NOT NULL,
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE tokens (
    token      TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    agent      TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX tokens_user ON tokens(user_id);

CREATE TABLE channels (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    topic      TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY,
    channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    author_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    content    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    edited_at  INTEGER,
    nonce      TEXT
);
-- History pages walk (channel, id) backwards; this is the index that makes that a
-- range scan instead of a sort.
CREATE INDEX messages_channel ON messages(channel_id, id DESC);
-- The idempotency guard for resends. Partial, so the many older messages with no
-- nonce do not all collide on NULL.
CREATE UNIQUE INDEX messages_nonce ON messages(author_id, nonce) WHERE nonce IS NOT NULL;

CREATE TABLE attachments (
    id           INTEGER PRIMARY KEY,
    -- NULL until a message claims it. An upload that is never sent stays
    -- unattached and expires on the same three-day clock as everything else.
    message_id   INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    uploader_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    size         INTEGER NOT NULL,
    content_type TEXT    NOT NULL,
    width        INTEGER NOT NULL DEFAULT 0,
    height       INTEGER NOT NULL DEFAULT 0,
    -- The blob's name on disk, and the client's cache key. Several rows can share
    -- one: uploads are deduplicated by content.
    sha256       TEXT    NOT NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    -- Set by the janitor. The row outlives the bytes on purpose.
    blob_deleted INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX attachments_message ON attachments(message_id);
CREATE INDEX attachments_expiry ON attachments(expires_at, blob_deleted);
CREATE INDEX attachments_sha ON attachments(sha256);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn user(db: &Db, name: &str) -> User {
        db.create_user(name, name, "hash").unwrap()
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let db = db();
        db.migrate().unwrap();
        db.migrate().unwrap();
        assert!(db.is_empty().unwrap());
    }

    #[test]
    fn login_names_are_unique_regardless_of_case() {
        let db = db();
        user(&db, "ada");
        let err = db.create_user("ADA", "ADA", "hash").unwrap_err();
        assert!(err.to_string().contains("taken"), "{err}");
        // And a lookup finds the account whatever case was typed.
        assert!(db.user_by_name("AdA").unwrap().is_some());
    }

    #[test]
    fn tokens_resolve_to_their_user_and_stop_when_revoked() {
        let db = db();
        let ada = user(&db, "ada");
        db.store_token("secret", ada.id, "test").unwrap();
        assert_eq!(db.user_for_token("secret").unwrap().unwrap().id, ada.id);
        db.revoke_token("secret").unwrap();
        assert!(db.user_for_token("secret").unwrap().is_none());
        assert!(db.user_for_token("never issued").unwrap().is_none());
    }

    /// The resend guard. This is the behaviour that lets a client retry a send it
    /// never got an answer to, which it otherwise cannot do without risking
    /// doubles.
    #[test]
    fn the_same_nonce_posts_once() {
        let db = db();
        let ada = user(&db, "ada");
        let general = db.create_channel("general", ChannelKind::Text).unwrap();

        let first = db.insert_message(general.id, ada.id, "hello", Some("n1"), &[]).unwrap();
        let again = db.insert_message(general.id, ada.id, "hello", Some("n1"), &[]).unwrap();
        assert_eq!(first.id, again.id);

        // A different author with the same nonce is a different message: nonces are
        // only unique per sender, since two clients pick them independently.
        let bob = user(&db, "bob");
        let bobs = db.insert_message(general.id, bob.id, "hello", Some("n1"), &[]).unwrap();
        assert_ne!(first.id, bobs.id);

        // And no nonce means no deduplication — two identical messages sent
        // deliberately must both appear.
        let a = db.insert_message(general.id, ada.id, "ha", None, &[]).unwrap();
        let b = db.insert_message(general.id, ada.id, "ha", None, &[]).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn history_pages_backwards_and_says_when_it_reached_the_start() {
        let db = db();
        let ada = user(&db, "ada");
        let general = db.create_channel("general", ChannelKind::Text).unwrap();
        let ids: Vec<Id> = (0..10)
            .map(|i| {
                db.insert_message(general.id, ada.id, &format!("m{i}"), None, &[])
                    .unwrap()
                    .id
            })
            .collect();

        let (page, complete) = db.history(general.id, None, 4).unwrap();
        assert_eq!(page.len(), 4);
        assert!(!complete, "there are six older messages");
        // Oldest first, and the page is the *newest* four.
        assert_eq!(page.first().unwrap().id, ids[6]);
        assert_eq!(page.last().unwrap().id, ids[9]);

        let (older, complete) = db.history(general.id, Some(ids[6]), 4).unwrap();
        assert_eq!(older.iter().map(|m| m.id).collect::<Vec<_>>(), ids[2..6]);
        assert!(!complete);

        let (oldest, complete) = db.history(general.id, Some(ids[2]), 4).unwrap();
        assert_eq!(oldest.iter().map(|m| m.id).collect::<Vec<_>>(), ids[0..2]);
        assert!(complete, "that page reached the beginning");

        let (none, complete) = db.history(general.id, Some(ids[0]), 4).unwrap();
        assert!(none.is_empty());
        assert!(complete);
    }

    #[test]
    fn a_history_request_cannot_ask_for_the_whole_channel() {
        let db = db();
        let ada = user(&db, "ada");
        let general = db.create_channel("general", ChannelKind::Text).unwrap();
        for i in 0..(HISTORY_MAX as usize + 20) {
            db.insert_message(general.id, ada.id, &format!("m{i}"), None, &[]).unwrap();
        }
        let (page, _) = db.history(general.id, None, u16::MAX).unwrap();
        assert_eq!(page.len(), HISTORY_MAX as usize);
        // Zero is also nonsense, and clamps up rather than returning nothing.
        let (page, _) = db.history(general.id, None, 0).unwrap();
        assert_eq!(page.len(), 1);
    }

    #[test]
    fn only_the_author_can_edit_or_delete() {
        let db = db();
        let ada = user(&db, "ada");
        let bob = user(&db, "bob");
        let general = db.create_channel("general", ChannelKind::Text).unwrap();
        let message = db.insert_message(general.id, ada.id, "mine", None, &[]).unwrap();

        assert!(db.edit_message(message.id, bob.id, "yours").unwrap().is_none());
        assert!(db.delete_message(message.id, bob.id).unwrap().is_none());

        let edited = db.edit_message(message.id, ada.id, "mine, revised").unwrap().unwrap();
        assert_eq!(edited.content, "mine, revised");
        assert!(edited.edited_at.is_some());

        assert_eq!(db.delete_message(message.id, ada.id).unwrap(), Some(general.id));
        assert!(db.message(message.id).unwrap().is_none());
    }

    #[test]
    fn attachments_belong_to_their_uploader_and_to_one_message() {
        let db = db();
        let ada = user(&db, "ada");
        let bob = user(&db, "bob");
        let general = db.create_channel("general", ChannelKind::Text).unwrap();

        let mine = db
            .insert_attachment(ada.id, "a.png", 10, "image/png", 4, 4, &"aa".repeat(32))
            .unwrap();
        let theirs = db
            .insert_attachment(bob.id, "b.png", 10, "image/png", 4, 4, &"bb".repeat(32))
            .unwrap();

        // Attaching somebody else's upload is dropped, not honoured.
        let message = db
            .insert_message(general.id, ada.id, "look", None, &[mine.id, theirs.id])
            .unwrap();
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.attachments[0].id, mine.id);

        // And an already-attached upload cannot be attached again, which would
        // otherwise be a way to keep a blob alive past its three days.
        let second = db.insert_message(general.id, ada.id, "again", None, &[mine.id]).unwrap();
        assert!(second.attachments.is_empty());
    }

    /// The heart of the storage policy: the bytes go, the row stays.
    #[test]
    fn expiry_drops_blobs_but_keeps_the_metadata() {
        let db = db();
        let ada = user(&db, "ada");
        let sha = "cc".repeat(32);
        let old = db.insert_attachment(ada.id, "old.png", 9, "image/png", 2, 2, &sha).unwrap();

        // Nothing is due yet.
        assert!(db.expired_blobs(old.expires_at - 1).unwrap().is_empty());

        let due = db.expired_blobs(old.expires_at).unwrap();
        assert_eq!(due, vec![sha.clone()]);
        assert_eq!(db.mark_blob_deleted(&sha).unwrap(), 1);

        let (still_there, blob_gone) = db.attachment(old.id).unwrap().unwrap();
        assert!(blob_gone);
        assert_eq!(still_there.name, "old.png", "the client still needs the name");
        assert_eq!(still_there.sha256, sha, "and the cache key");
        assert_eq!((still_there.width, still_there.height), (2, 2));

        // Marked rows are not offered again, so the janitor does not retry a file
        // it already removed on every pass.
        assert!(db.expired_blobs(old.expires_at).unwrap().is_empty());
    }

    /// Deduplication makes expiry subtler than "delete what is old": the same image
    /// posted again shares one file, and the newer post's three days have to win.
    #[test]
    fn a_shared_blob_survives_until_its_last_reference_expires() {
        let db = db();
        let ada = user(&db, "ada");
        let sha = "dd".repeat(32);
        let first = db.insert_attachment(ada.id, "x.png", 9, "image/png", 2, 2, &sha).unwrap();

        // A second attachment with the same content and a later expiry.
        {
            let conn = db.lock();
            conn.execute(
                "INSERT INTO attachments
                   (uploader_id, name, size, content_type, width, height, sha256, created_at, expires_at)
                 VALUES (?1, 'x.png', 9, 'image/png', 2, 2, ?2, 0, ?3)",
                params![ada.id.0 as i64, sha, first.expires_at + 10_000],
            )
            .unwrap();
        }

        assert!(
            db.expired_blobs(first.expires_at).unwrap().is_empty(),
            "the newer post still needs the file"
        );
        assert_eq!(db.expired_blobs(first.expires_at + 10_000).unwrap(), vec![sha]);
    }

    #[test]
    fn deleting_a_message_orphans_its_blob_for_the_janitor() {
        let db = db();
        let ada = user(&db, "ada");
        let general = db.create_channel("general", ChannelKind::Text).unwrap();
        let sha = "ee".repeat(32);
        let a = db.insert_attachment(ada.id, "x.png", 9, "image/png", 2, 2, &sha).unwrap();
        db.insert_message(general.id, ada.id, "look", None, &[a.id]).unwrap();

        assert!(db.orphaned_blobs(std::slice::from_ref(&sha)).unwrap().is_empty());

        let message = db.history(general.id, None, 10).unwrap().0.pop().unwrap();
        db.delete_message(message.id, ada.id).unwrap();
        assert_eq!(db.orphaned_blobs(std::slice::from_ref(&sha)).unwrap(), vec![sha]);
    }

    #[test]
    fn channels_get_a_position_within_their_kind() {
        let db = db();
        let general = db.create_channel("general", ChannelKind::Text).unwrap();
        let random = db.create_channel("random", ChannelKind::Text).unwrap();
        let lounge = db.create_channel("Lounge", ChannelKind::Voice).unwrap();
        assert_eq!((general.position, random.position), (0, 1));
        assert_eq!(lounge.position, 0, "voice channels count separately");

        let listed = db.channels().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(db.channel(lounge.id).unwrap().unwrap().kind, ChannelKind::Voice);
    }

    #[test]
    fn an_unknown_channel_kind_reads_as_text_rather_than_vanishing() {
        assert_eq!(parse_kind("text"), ChannelKind::Text);
        assert_eq!(parse_kind("voice"), ChannelKind::Voice);
        assert_eq!(parse_kind("holodeck"), ChannelKind::Text);
    }
}
