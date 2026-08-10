//! The wire protocol both halves of BoaVoice speak.
//!
//! There are two planes and they are deliberately different shapes.
//!
//! The **control plane** ([`control`]) is JSON over one WebSocket per client.
//! Everything that is a fact — who exists, what was said, who is in which voice
//! channel — travels here, in order, over TCP, and is worth being able to read in
//! a log. A binary encoding would save a few kilobytes per session and cost every
//! future debugging session.
//!
//! The **media plane** ([`media`]) is a fixed 16-byte header plus an AEAD-sealed
//! payload over UDP. Voice is only useful if it is *late by very little*: a
//! retransmitted 20 ms frame arrives after the moment it belonged to, so loss is
//! better than delay and TCP is the wrong tool. The header stays in the clear
//! because the relay routes on it.
//!
//! Both planes are versioned by [`PROTOCOL_VERSION`]. The server refuses a client
//! whose version it does not know rather than guessing at a payload — a protocol
//! mismatch that half-works is much harder to diagnose than one that is rejected
//! at the door.

pub mod control;
pub mod media;
pub mod model;

pub use control::{ClientMsg, ServerMsg};
pub use media::{MediaKind, MediaError, PacketHeader, SessionKey};
pub use model::*;

/// The protocol revision. Bumped on any change to [`control`] or [`media`] that
/// an older peer could not handle.
pub const PROTOCOL_VERSION: u16 = 1;

/// How long the server keeps an uploaded attachment blob before deleting it.
///
/// Three days. The point of the whole attachment design: a self-hosted box should
/// not have to grow forever because somebody pasted screenshots into a channel for
/// a year. The *message* keeps its attachment metadata permanently, so a client
/// that saw the image still shows it from its own cache — the server is a
/// three-day courier, not an archive.
pub const ATTACHMENT_TTL_SECS: u64 = 3 * 24 * 60 * 60;

/// A server-assigned identifier.
///
/// One opaque `u64` for users, channels and messages alike. They come from a
/// single per-kind sequence in SQLite, so ids are dense and monotonic — message
/// ids therefore sort by creation time, which is what lets history paging say
/// "before this id" without a second timestamp column to disagree with.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    Default,
)]
#[serde(transparent)]
pub struct Id(pub u64);

impl Id {
    /// The id no row ever has, for "nothing selected".
    pub const NONE: Id = Id(0);

    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Id(value)
    }
}

/// Milliseconds since the Unix epoch, as the protocol carries time.
///
/// Not `SystemTime`: it has to serialise identically on three platforms and be
/// comparable in SQLite, and a signed 64-bit millisecond count does both. Signed
/// because a clock skewed behind 1970 should produce a negative timestamp rather
/// than a number two hundred million years in the future.
pub type Millis = i64;

/// Now, in [`Millis`].
pub fn now_millis() -> Millis {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as Millis,
        Err(err) => -(err.duration().as_millis() as Millis),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialise_as_bare_numbers() {
        // `#[serde(transparent)]` matters for the JSON to stay readable, and for a
        // future non-Rust client to not have to know about a wrapper.
        assert_eq!(serde_json::to_string(&Id(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<Id>("7").unwrap(), Id(7));
    }

    #[test]
    fn ids_order_by_value_so_history_paging_can_use_them() {
        let mut ids = vec![Id(3), Id(1), Id(2)];
        ids.sort();
        assert_eq!(ids, vec![Id(1), Id(2), Id(3)]);
        assert!(Id::NONE.is_none());
        assert!(!Id(1).is_none());
    }

    #[test]
    fn the_attachment_ttl_is_three_days() {
        assert_eq!(ATTACHMENT_TTL_SECS, 259_200);
    }
}
