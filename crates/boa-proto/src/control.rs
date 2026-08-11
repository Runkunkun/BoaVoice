//! The control plane: one WebSocket per client, JSON in both directions.
//!
//! Both enums are internally tagged on `"t"`, so a frame reads as
//! `{"t":"send_message","channel":3,...}` — one line in a log that says what it is
//! without a decoder. Unknown variants are a hard parse error rather than being
//! skipped: a client that silently drops messages it does not understand looks
//! like it is working right up until the moment somebody needed the one it
//! dropped.
//!
//! Two rules shape what is in here.
//!
//! **Facts flow one way.** The client asks; the server decides and announces. There
//! is no message that lets a client tell others something directly, which is what
//! keeps the server's view authoritative and means a hostile client can lie about
//! nothing but its own input.
//!
//! **Nothing per-frame.** The control plane carries state changes, never streams.
//! The most frequent message here is [`ServerMsg::Speaking`], at roughly one per
//! talk-spurt; audio itself never touches it. A design that pushed voice activity
//! per 20 ms frame would put a hundred WebSocket writes per second per person on a
//! TCP connection shared with chat, and stall chat behind it.

use serde::{Deserialize, Serialize};

use crate::{Attachment, Channel, FileOffer, Id, Message, ScreenShare, ServerInfo, User, VoiceState};

/// What the sharer wants; the server answers with a [`ScreenShare`] carrying the
/// stream id it assigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    #[serde(default)]
    pub with_audio: bool,
}

/// Client to server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First frame on every connection. The server replies [`ServerMsg::Ready`] or
    /// closes; nothing else is accepted before it.
    Identify {
        token: String,
        protocol_version: u16,
        /// Free-form, for the server log: `"boa-client 0.1.0 macos"`.
        #[serde(default)]
        agent: String,
    },

    /// Ask for a page of a channel's past, oldest-first in the reply. `before` is
    /// exclusive; `None` means "the most recent page".
    History {
        channel: Id,
        #[serde(default)]
        before: Option<Id>,
        limit: u16,
    },

    SendMessage {
        channel: Id,
        content: String,
        /// The client's own id for this message, echoed back in
        /// [`Message::nonce`]. Also the server's duplicate guard: a resend after a
        /// reconnect carries the same nonce and is answered with the original
        /// message rather than posting it twice.
        nonce: String,
        /// Ids from a prior HTTP upload. Uploading over HTTP rather than through
        /// this socket keeps a 20 MB image out of the path that voice-state changes
        /// and chat share, where it would head-of-line block both for seconds.
        #[serde(default)]
        attachments: Vec<Id>,
    },
    EditMessage {
        id: Id,
        content: String,
    },
    DeleteMessage {
        id: Id,
    },

    /// "I am typing in this channel." Fire-and-forget; the server fans it out and
    /// forgets it. Rate-limited server-side, because the obvious client
    /// implementation sends one per keystroke.
    Typing {
        channel: Id,
    },

    CreateChannel {
        name: String,
        kind: crate::ChannelKind,
    },
    SetDisplayName {
        name: String,
    },

    /// Join a voice channel. Answered by [`ServerMsg::VoiceReady`], which carries
    /// the session key the media plane needs. Joining a second channel leaves the
    /// first — being in two calls at once is not a state the mixer models.
    JoinVoice {
        channel: Id,
    },
    LeaveVoice,
    /// Microphone and output switches. Sent on change, not per frame.
    UpdateVoiceState {
        muted: bool,
        deafened: bool,
    },
    /// Talk-spurt boundaries, from the client's own voice detection. The server
    /// forwards it so other clients can light up a name without having to decode
    /// audio to find out whether it contains speech.
    Speaking {
        speaking: bool,
    },

    StartScreen(ScreenRequest),
    StopScreen,
    /// Subscribe to somebody's screen. The relay sends video only to subscribers,
    /// so a channel of ten people watching nothing costs the sharer's uplink
    /// nothing.
    WatchScreen {
        user: Id,
    },
    UnwatchScreen {
        user: Id,
    },
    /// How a share is arriving, sent by a watcher about once a second.
    ///
    /// **The one message that makes a share usable on a real connection.** Without it a sender
    /// transmits whatever bitrate it was configured with and never learns that half of it is being
    /// discarded — so a link that cannot carry the configured rate stays broken for as long as the
    /// share lasts, and no amount of cleverness at the receiving end can help. With it the sender can
    /// do what every video call does: come down until the picture arrives, then feel its way back up.
    ///
    /// Counted since the previous report rather than cumulatively, so a report that goes missing
    /// costs one second of information rather than skewing the total for the rest of the call.
    ScreenReport {
        /// Whose share is being reported on.
        user: Id,
        /// Pictures that arrived whole and decoded.
        received: u32,
        /// Pictures that did not — a missing fragment, or no room in the decoder's queue.
        lost: u32,
        /// Nothing has been decodable yet. Somebody who has just started watching, or a stream whose
        /// reference chain was broken by loss: both need a keyframe rather than more delta frames.
        want_keyframe: bool,
    },

    /// Offer a file directly to another user. The server relays the offer and
    /// stays out of the transfer — see [`FileOffer`].
    OfferFile {
        to: Id,
        offer: FileOffer,
    },
    /// Withdraw an offer that was not accepted, so the recipient's UI can stop
    /// showing a code that will no longer connect.
    CancelFileOffer {
        to: Id,
        code: String,
    },

    /// Application-level keepalive. The WebSocket has its own ping, but a client
    /// that wants to *measure* the round trip needs one it can match to a reply.
    Ping {
        nonce: u64,
    },
}

/// Server to client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Everything needed to draw the app, in one frame. Sent once per connection.
    ///
    /// One big frame rather than a stream of small ones because a half-populated
    /// window is worse than a slightly later complete one, and because every
    /// incremental design ends up needing a "now you have it all" marker anyway.
    Ready {
        user: User,
        server: ServerInfo,
        users: Vec<User>,
        channels: Vec<Channel>,
        voice_states: Vec<VoiceState>,
    },

    MessageCreate(Message),
    MessageUpdate(Message),
    MessageDelete {
        channel: Id,
        id: Id,
    },
    /// A page from [`ClientMsg::History`], oldest first. `complete` is set when
    /// this page reached the beginning of the channel, so the client can stop
    /// asking rather than discovering it by getting an empty page.
    History {
        channel: Id,
        messages: Vec<Message>,
        complete: bool,
    },

    Typing {
        channel: Id,
        user: Id,
    },
    Presence {
        user: Id,
        online: bool,
    },
    UserUpdate(User),
    ChannelCreate(Channel),

    /// The media plane's credentials for this voice session.
    ///
    /// The key is generated by the server and given to every member of the
    /// channel, which is transport encryption rather than end-to-end: it stops
    /// anyone on the network path from listening, and does not stop the server. A
    /// relay that could not decrypt would be strictly better and needs per-pair
    /// key agreement; that is a later change and is called out in the README rather
    /// than glossed over here.
    VoiceReady {
        channel: Id,
        /// This client's own stream id.
        ssrc: u32,
        /// 32 bytes, base64. See [`crate::SessionKey::from_base64`].
        key: String,
        /// Where to send media. The host is the one the control connection used.
        media_port: u16,
    },
    /// Somebody's voice state: sent on join and on every change afterwards.
    VoiceState(VoiceState),
    VoiceLeave {
        user: Id,
        channel: Id,
    },
    Speaking {
        user: Id,
        speaking: bool,
    },

    ScreenStart {
        user: Id,
        share: ScreenShare,
    },
    ScreenStop {
        user: Id,
    },
    /// A watcher's report, forwarded to the person doing the sharing.
    ///
    /// The server does not interpret it — it checks that the reporter is watching this share and
    /// passes it on. Deciding what to do about loss belongs to whoever owns the encoder.
    ScreenReport {
        /// Who is watching.
        from: Id,
        received: u32,
        lost: u32,
        want_keyframe: bool,
    },

    FileOffer {
        from: Id,
        offer: FileOffer,
    },
    FileOfferCancelled {
        from: Id,
        code: String,
    },

    /// An upload finished; sent to the uploader only. The client needs the
    /// server-assigned id and expiry before it can reference the file in
    /// [`ClientMsg::SendMessage`], and gets them here when the upload was started
    /// out of band.
    AttachmentReady(Attachment),

    /// Something went wrong with the last thing this client asked for.
    ///
    /// `fatal` distinguishes "that request failed" from "this connection is over" —
    /// without it a client cannot tell whether to retry or to go back to the login
    /// screen, and guessing wrong means either a hang or a spurious logout.
    Error {
        code: ErrorCode,
        message: String,
        #[serde(default)]
        fatal: bool,
    },

    Pong {
        nonce: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Bad or expired token, or a second `Identify` on the same connection.
    Unauthorised,
    /// The server does not speak this [`crate::PROTOCOL_VERSION`].
    VersionMismatch,
    /// Referred to a channel, user or message that does not exist or is not
    /// visible.
    NotFound,
    /// Well-formed but not allowed — an edit of somebody else's message.
    Forbidden,
    /// Malformed frame, or a field outside its permitted range.
    BadRequest,
    /// Too fast. The client should back off rather than retry immediately.
    RateLimited,
    /// The server broke, not the client. Details are in the server's log, not here.
    Internal,
}

impl ServerMsg {
    /// Convenience for the server's error paths, which are the ones most likely to
    /// be written in a hurry.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMsg::Error { code, message: message.into(), fatal: false }
    }

    /// An error that also ends the connection.
    pub fn fatal(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMsg::Error { code, message: message.into(), fatal: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelKind;

    /// Round-trip every variant shape the enum has — struct, newtype and unit —
    /// because internally-tagged serde treats the three differently and a unit
    /// variant in particular is easy to get wrong (it must still be an object with
    /// a `t`, not a bare string).
    #[test]
    fn all_variant_shapes_round_trip() {
        let cases: Vec<ClientMsg> = vec![
            ClientMsg::Identify {
                token: "t".into(),
                protocol_version: crate::PROTOCOL_VERSION,
                agent: "test".into(),
            },
            ClientMsg::LeaveVoice,
            ClientMsg::StartScreen(ScreenRequest {
                width: 1920,
                height: 1080,
                fps: 60,
                kbps: 12_000,
                with_audio: true,
            }),
            ClientMsg::CreateChannel { name: "general".into(), kind: ChannelKind::Text },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).unwrap();
            assert!(json.starts_with('{'), "{json}");
            assert_eq!(serde_json::from_str::<ClientMsg>(&json).unwrap(), case);
        }
    }

    #[test]
    fn the_tag_is_snake_case_and_lives_in_t() {
        let json = serde_json::to_string(&ClientMsg::LeaveVoice).unwrap();
        assert_eq!(json, r#"{"t":"leave_voice"}"#);
        let json = serde_json::to_string(&ClientMsg::Ping { nonce: 9 }).unwrap();
        assert_eq!(json, r#"{"t":"ping","nonce":9}"#);
    }

    /// A frame the server does not know must fail loudly. The alternative — an
    /// `#[serde(other)]` catch-all — turns a protocol mistake into silence.
    #[test]
    fn unknown_variants_are_rejected() {
        assert!(serde_json::from_str::<ClientMsg>(r#"{"t":"do_a_barrel_roll"}"#).is_err());
        assert!(serde_json::from_str::<ClientMsg>(r#"{"t":"ping"}"#).is_err(), "nonce is required");
    }

    #[test]
    fn errors_carry_whether_the_connection_survived() {
        let ServerMsg::Error { fatal, .. } = ServerMsg::error(ErrorCode::NotFound, "no such channel")
        else {
            panic!("not an error")
        };
        assert!(!fatal);
        let ServerMsg::Error { fatal, .. } = ServerMsg::fatal(ErrorCode::Unauthorised, "bad token")
        else {
            panic!("not an error")
        };
        assert!(fatal);
    }
}
