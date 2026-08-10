//! The things a BoaVoice server contains, as both halves see them.
//!
//! These structs are the payloads of [`crate::control`] messages *and* the shape
//! the client keeps in memory, which is on purpose: one definition means a field
//! the server starts sending cannot be quietly ignored by a parallel client-side
//! copy that nobody updated.

use serde::{Deserialize, Serialize};

use crate::{Id, Millis};

/// Someone with an account on this server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    /// The login name, unique and lowercase-folded by the server.
    pub name: String,
    /// What other people see. Free-form, changeable, not unique.
    pub display_name: String,
    /// Sent by the server on every `Ready`; not a fact the client can set.
    #[serde(default)]
    pub online: bool,
}

impl User {
    /// The name to draw. Display names are allowed to be empty; falling back here
    /// rather than at every call site means a blank one cannot produce a row with
    /// no label at all.
    pub fn label(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.name
        } else {
            &self.display_name
        }
    }
}

/// What a channel is for.
///
/// Voice and text are separate channels rather than one channel with both, which
/// is Discord's model and the one people expect. A `Voice` channel still carries
/// messages — the little chat that belongs to a call — so the distinction is about
/// what the sidebar offers to join, not about which tables exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub kind: ChannelKind,
    /// Sidebar order. Ties break by id, so a server that never sets this still
    /// lists channels in creation order rather than in whatever order SQLite felt
    /// like returning.
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub topic: String,
}

/// A file that came with a message.
///
/// The `url` is *not* stored here — it is derived from the id, because a stored
/// URL would embed the host name the uploader happened to use and break for
/// everyone reaching the same server by another route (LAN address, Tailscale
/// name, reverse proxy).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Id,
    pub name: String,
    pub size: u64,
    pub content_type: String,
    /// Pixel size, when the server could work it out. Lets the client reserve the
    /// right space in the chat log before the bytes arrive, so images do not shove
    /// the scroll position around as they load.
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// BLAKE3-free: a plain SHA-256, hex. The client uses it as its cache key, so
    /// the same image posted twice is stored once locally, and a cached copy can be
    /// verified against the message even after the server has dropped the blob.
    pub sha256: String,
    /// When the server will have deleted its copy. After this, only clients that
    /// downloaded it still have it — see [`crate::ATTACHMENT_TTL_SECS`].
    pub expires_at: Millis,
}

impl Attachment {
    /// The path the blob is served from, relative to the server root.
    pub fn path(&self) -> String {
        format!("/attachments/{}", self.id)
    }

    /// Whether this looks like something to draw inline rather than list as a file.
    pub fn is_image(&self) -> bool {
        matches!(
            self.content_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
        )
    }

    pub fn expired_at(&self, now: Millis) -> bool {
        now >= self.expires_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub channel: Id,
    pub author: Id,
    pub content: String,
    pub created_at: Millis,
    #[serde(default)]
    pub edited_at: Option<Millis>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Echoed back from [`crate::ClientMsg::SendMessage`] so the sender can match
    /// the server's copy to the one it optimistically drew, and replace rather than
    /// duplicate it. Absent on everyone else's copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

/// What somebody is doing in a voice channel.
///
/// `speaking` is in here but is *not* sent on the control plane per talk-spurt —
/// see [`crate::control::ServerMsg::Speaking`], which is the cheap per-spurt
/// message. This struct is the durable part: where you are and what you have
/// switched off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceState {
    pub user: Id,
    pub channel: Id,
    /// Microphone off: nothing is captured, nothing is sent.
    pub muted: bool,
    /// Everyone else off: nothing is played, and — because there is no point
    /// paying for the uplink either — nothing is sent, so deafened implies muted
    /// on the wire. The two flags stay separate in the UI because unmuting from
    /// deafened should restore the microphone state you had.
    pub deafened: bool,
    /// The sender's stream identifier, so a receiver can attribute packets to a
    /// person. Assigned by the server on join.
    pub ssrc: u32,
    /// Set while this person is sharing their screen.
    #[serde(default)]
    pub screen: Option<ScreenShare>,
}

impl VoiceState {
    /// Whether this state means "send no audio".
    pub fn silent(&self) -> bool {
        self.muted || self.deafened
    }
}

/// A screen share in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenShare {
    /// A second stream identifier: video from the same person is a separate stream
    /// from their voice, so a viewer can subscribe to one without the other.
    pub ssrc: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Kilobits per second the sharer is targeting.
    ///
    /// There is no server-side ceiling on this, and that is the point of the
    /// project: quality is a setting on the machine doing the encoding, not a
    /// subscription tier. A self-hosted box's limit is its own uplink.
    pub kbps: u32,
    /// True when what is being captured includes the desktop audio.
    #[serde(default)]
    pub with_audio: bool,
}

/// What the client is told about the server it just connected to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    /// UDP port for the media plane. The host is whatever the client used for the
    /// control connection — a server behind a reverse proxy still has to expose
    /// this port directly, since it is not HTTP.
    pub media_port: u16,
    pub protocol_version: u16,
    /// How long attachments survive here. Sent rather than assumed, so a server
    /// that has been configured differently does not make its clients lie about
    /// when an image will vanish.
    pub attachment_ttl_secs: u64,
    /// Largest upload the server will accept, in bytes.
    pub max_upload_bytes: u64,
    /// The rendezvous server for direct file transfers, if this server runs or
    /// recommends one. Clients fall back to the public default when absent.
    #[serde(default)]
    pub wormhole_rendezvous: Option<String>,
    /// The transit relay for the same, used when a direct connection between two
    /// peers cannot be established.
    #[serde(default)]
    pub wormhole_transit: Option<String>,
}

/// An offer to send a file directly, peer to peer, bypassing the server.
///
/// The server relays this one small message and then has nothing to do with the
/// transfer: the bytes go over a connection the two clients negotiate through
/// magic-wormhole, encrypted with a key derived from the code below. The server
/// never sees the file, and — since a wormhole code is single-use and short-lived
/// — learning the code from this relay does not help it either, because the first
/// party to claim it wins and that is the intended recipient.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    /// The wormhole code: `<nameplate>-<word>-<word>`.
    pub code: String,
    pub name: String,
    pub size: u64,
    /// Which channel the offer was made in, so the recipient's UI can show it in
    /// context rather than as a bare popup.
    pub channel: Id,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_display_name_falls_back_to_the_login_name() {
        let mut user = User {
            id: Id(1),
            name: "ada".into(),
            display_name: String::new(),
            online: true,
        };
        assert_eq!(user.label(), "ada");
        user.display_name = "   ".into();
        assert_eq!(user.label(), "ada", "whitespace is not a display name");
        user.display_name = "Ada L.".into();
        assert_eq!(user.label(), "Ada L.");
    }

    #[test]
    fn attachment_urls_are_derived_not_stored() {
        let a = Attachment {
            id: Id(42),
            name: "shot.png".into(),
            size: 1234,
            content_type: "image/png".into(),
            width: 800,
            height: 600,
            sha256: "00".repeat(32),
            expires_at: 1_000,
        };
        assert_eq!(a.path(), "/attachments/42");
        assert!(a.is_image());
        assert!(a.expired_at(1_000), "expiry is inclusive of its own instant");
        assert!(!a.expired_at(999));
    }

    #[test]
    fn deafened_implies_no_uplink() {
        let base = VoiceState {
            user: Id(1),
            channel: Id(2),
            muted: false,
            deafened: false,
            ssrc: 7,
            screen: None,
        };
        assert!(!base.silent());
        assert!(VoiceState { muted: true, ..base }.silent());
        // The interesting one: deafened stops capture too, because sending audio
        // into a call you cannot hear is pure waste.
        assert!(VoiceState { deafened: true, ..base }.silent());
    }

    /// Older clients must survive new optional fields, which is what all those
    /// `#[serde(default)]`s are for. This test pins that a minimal object — the
    /// shape a hand-written or older peer might send — still parses.
    #[test]
    fn optional_fields_are_actually_optional() {
        let json = r#"{"id":1,"channel":2,"author":3,"content":"hi","created_at":0}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(msg.attachments.is_empty());
        assert_eq!(msg.edited_at, None);
        assert_eq!(msg.nonce, None);

        let json = r#"{"id":1,"name":"general","kind":"text"}"#;
        let channel: Channel = serde_json::from_str(json).unwrap();
        assert_eq!(channel.position, 0);
        assert_eq!(channel.kind, ChannelKind::Text);
    }

    /// The nonce is skipped when absent rather than serialised as `null`, because
    /// every message on a busy channel carries it and `"nonce":null` on each one
    /// is pure wire noise.
    #[test]
    fn a_message_without_a_nonce_omits_the_field() {
        let msg = Message {
            id: Id(1),
            channel: Id(2),
            author: Id(3),
            content: "hi".into(),
            created_at: 0,
            edited_at: None,
            attachments: vec![],
            nonce: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("nonce"), "{json}");
    }
}
