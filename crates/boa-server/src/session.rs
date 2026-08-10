//! One client's control connection, from `Identify` to hang-up.
//!
//! The shape is a reader task and a writer task over one split WebSocket, joined by
//! an unbounded channel. Two reasons for the channel rather than writing straight to
//! the socket from wherever an event happens:
//!
//! * **The hub must not await.** Fanning a message out to twenty connections means
//!   twenty sends, and if a send could block on a slow client's socket, then one
//!   person on hotel wifi would stall everybody else's chat — while holding the hub's
//!   mutex.
//! * **Ordering.** Each connection has exactly one writer, so frames arrive in the
//!   order they were produced, which is what lets the client apply them blindly
//!   instead of reconciling.
//!
//! The channel is unbounded, which is a real if bounded risk: a client that stops
//! reading grows its queue until the TCP connection times out. Bounding it would mean
//! choosing between blocking the hub (see above) and dropping events, and a client
//! that has silently missed events is worse than one that gets disconnected. The
//! WebSocket's own keepalive is what puts a ceiling on how long that can go on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use boa_proto::control::ErrorCode;
use boa_proto::{ChannelKind, ClientMsg, Id, ServerMsg, User};
use futures_util::{SinkExt as _, StreamExt as _};

use crate::db::HISTORY_MAX;
use crate::hub::{ConnId, Hub};

/// Longest a client may take to send its first frame.
///
/// A connection that has not identified holds a socket and a task and is not yet
/// attributable to anybody, which makes it the cheapest thing for a stranger to
/// open thousands of.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest message the server will store.
const MAX_CONTENT: usize = 4_000;

/// Most attachments on one message.
const MAX_ATTACHMENTS: usize = 10;

/// How often one client's typing notice is forwarded per channel.
///
/// The obvious client implementation sends one per keystroke. Three seconds is well
/// inside the few-second window a receiver shows the indicator for, so throttling
/// here is invisible and turns a hundred frames a minute into twenty.
const TYPING_INTERVAL: Duration = Duration::from_secs(3);

/// Longest a display name may be.
const MAX_DISPLAY_NAME: usize = 48;

/// The halves of a split WebSocket, named because they appear in three signatures and
/// the full paths make those unreadable.
type WsSink = futures_util::stream::SplitSink<WebSocket, Message>;
type WsStream = futures_util::stream::SplitStream<WebSocket>;

/// Send one last frame and hang up.
///
/// Used only before a connection has identified, where there is no writer task yet and
/// so no channel to send through.
async fn refuse(sink: &mut WsSink, msg: ServerMsg) {
    if let Ok(text) = serde_json::to_string(&msg) {
        let _ = sink.send(Message::Text(text.into())).await;
    }
    let _ = sink.close().await;
}

/// Serve one connection. Returns when the socket closes.
pub async fn run(hub: Arc<Hub>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    let user = match identify(&hub, &mut stream, &mut sink).await {
        Some(user) => user,
        None => return,
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (conn, first_connection) = hub.connect(user.id, tx);
    log::info!("{}: connected ({conn:?})", user.name);

    // The writer. Owns the sink for the rest of the connection, which is what makes
    // "one writer per connection" true rather than merely intended.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(text) => text,
                Err(err) => {
                    // Unreachable in practice; a serialisation failure here would be a
                    // bug in a `Serialize` impl rather than anything about this client.
                    log::error!("serialising {msg:?}: {err}");
                    continue;
                }
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        // Closing explicitly rather than dropping: a client waiting on the socket
        // otherwise learns nothing until its own keepalive expires.
        let _ = sink.close().await;
    });

    let mut session = Session { hub: hub.clone(), conn, user: user.clone(), last_typing: HashMap::new() };
    session.send_ready();
    if first_connection {
        hub.broadcast_except(conn, ServerMsg::Presence { user: user.id, online: true });
    }

    while let Some(frame) = stream.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(err) => {
                log::debug!("{}: socket error: {err}", session.user.name);
                break;
            }
        };
        match frame {
            Message::Text(text) => {
                match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(msg) => {
                        if session.handle(msg).await.is_break() {
                            break;
                        }
                    }
                    Err(err) => {
                        // The frame is reported but not logged in full: it may contain
                        // a message somebody typed, and the server's log is not the
                        // place for that.
                        log::debug!("{}: unparseable frame: {err}", session.user.name);
                        session.send(ServerMsg::error(
                            ErrorCode::BadRequest,
                            format!("could not read that frame: {err}"),
                        ));
                    }
                }
            }
            // Binary frames are not part of the protocol. Attachments go over HTTP and
            // media over UDP, both for good reasons; accepting bytes here would invite
            // a third path that has neither's properties.
            Message::Binary(_) => session.send(ServerMsg::error(
                ErrorCode::BadRequest,
                "the control plane is text only",
            )),
            Message::Close(_) => break,
            // axum answers pings itself.
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    // Teardown, in the order the client's peers need to see it.
    let last_connection = hub.disconnect(conn);
    if last_connection {
        if hub.voice_channel_of(user.id).is_some() {
            let sharing = hub.stop_screen(user.id).is_some();
            if let Some(channel) = hub.leave_voice(user.id) {
                if sharing {
                    hub.broadcast(ServerMsg::ScreenStop { user: user.id });
                }
                hub.broadcast(ServerMsg::VoiceLeave { user: user.id, channel });
            }
        }
        hub.broadcast(ServerMsg::Presence { user: user.id, online: false });
    }
    writer.abort();
    log::info!("{}: disconnected", user.name);
}

/// Read and check the first frame.
///
/// Anything other than a valid `Identify` closes the connection. In particular a
/// *second* `Identify` later is refused by [`Session::handle`], because a connection
/// that could change identity mid-flight would make every authorisation check
/// conditional on when it ran.
async fn identify(hub: &Hub, stream: &mut WsStream, sink: &mut WsSink) -> Option<User> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        let frame = tokio::select! {
            frame = stream.next() => frame,
            _ = &mut deadline => {
                log::debug!("a connection never identified");
                return None;
            }
        };

        let text = match frame {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return None,
        };

        let first = match serde_json::from_str::<ClientMsg>(&text) {
            Ok(msg) => msg,
            Err(err) => {
                refuse(sink, ServerMsg::fatal(ErrorCode::BadRequest, format!("{err}"))).await;
                return None;
            }
        };

        let ClientMsg::Identify { token, protocol_version, agent } = first else {
            refuse(sink, ServerMsg::fatal(ErrorCode::Unauthorised, "identify first")).await;
            return None;
        };

        if protocol_version != boa_proto::PROTOCOL_VERSION {
            // Refused rather than attempted. A version mismatch that half-works is
            // much harder to diagnose than one that is turned away with a number in
            // the message.
            refuse(
                sink,
                ServerMsg::fatal(
                    ErrorCode::VersionMismatch,
                    format!(
                        "this server speaks protocol {}, the client speaks {protocol_version}",
                        boa_proto::PROTOCOL_VERSION
                    ),
                ),
            )
            .await;
            return None;
        }

        match hub.db.user_for_token(&token) {
            Ok(Some(user)) => {
                log::debug!("{}: identified ({agent})", user.name);
                return Some(user);
            }
            Ok(None) => {
                refuse(sink, ServerMsg::fatal(ErrorCode::Unauthorised, "that token is not valid"))
                    .await;
                return None;
            }
            Err(err) => {
                log::error!("looking up a token: {err:#}");
                refuse(sink, ServerMsg::fatal(ErrorCode::Internal, "could not check that token"))
                    .await;
                return None;
            }
        }
    }
}

struct Session {
    hub: Arc<Hub>,
    conn: ConnId,
    user: User,
    /// When this client's typing notice was last forwarded, per channel.
    last_typing: HashMap<Id, Instant>,
}

/// Whether the connection carries on.
use std::ops::ControlFlow;

impl Session {
    fn send(&self, msg: ServerMsg) {
        self.hub.send_to_conn(self.conn, msg);
    }

    fn fail(&self, code: ErrorCode, message: impl Into<String>) {
        self.send(ServerMsg::error(code, message));
    }

    /// The one place an internal error is turned into something a client can see.
    ///
    /// The detail goes to the log and a flat sentence goes to the client: a SQLite
    /// error message can name columns and paths, and that is information about the
    /// server rather than about the request.
    fn internal(&self, context: &str, err: anyhow::Error) {
        log::error!("{}: {context}: {err:#}", self.user.name);
        self.fail(ErrorCode::Internal, "the server could not do that");
    }

    fn send_ready(&self) {
        let (mut users, channels) = match (self.hub.db.users(), self.hub.db.channels()) {
            (Ok(users), Ok(channels)) => (users, channels),
            (Err(err), _) | (_, Err(err)) => {
                self.internal("building Ready", err);
                return;
            }
        };
        self.hub.decorate_presence(&mut users);

        let mut me = self.user.clone();
        me.online = true;
        self.send(ServerMsg::Ready {
            user: me,
            server: self.hub.config.server_info(),
            users,
            channels,
            voice_states: self.hub.voice_states(),
        });
    }

    async fn handle(&mut self, msg: ClientMsg) -> ControlFlow<()> {
        match msg {
            ClientMsg::Identify { .. } => {
                // A connection that could change identity would make every check
                // above depend on when it happened.
                self.send(ServerMsg::fatal(ErrorCode::Unauthorised, "already identified"));
                return ControlFlow::Break(());
            }

            ClientMsg::Ping { nonce } => self.send(ServerMsg::Pong { nonce }),

            ClientMsg::History { channel, before, limit } => {
                match self.hub.db.history(channel, before, limit.min(HISTORY_MAX)) {
                    Ok((messages, complete)) => {
                        self.send(ServerMsg::History { channel, messages, complete })
                    }
                    Err(err) => self.internal("reading history", err),
                }
            }

            ClientMsg::SendMessage { channel, content, nonce, attachments } => {
                let content = content.trim().to_string();
                if content.chars().count() > MAX_CONTENT {
                    self.fail(ErrorCode::BadRequest, format!("messages are limited to {MAX_CONTENT} characters"));
                    return ControlFlow::Continue(());
                }
                if attachments.len() > MAX_ATTACHMENTS {
                    self.fail(ErrorCode::BadRequest, format!("at most {MAX_ATTACHMENTS} attachments"));
                    return ControlFlow::Continue(());
                }
                // An empty message with no files is a stray keystroke, not something
                // to store and fan out.
                if content.is_empty() && attachments.is_empty() {
                    self.fail(ErrorCode::BadRequest, "nothing to send");
                    return ControlFlow::Continue(());
                }
                match self.hub.db.channel(channel) {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        self.fail(ErrorCode::NotFound, "no such channel");
                        return ControlFlow::Continue(());
                    }
                    Err(err) => {
                        self.internal("checking a channel", err);
                        return ControlFlow::Continue(());
                    }
                }

                let nonce = (!nonce.is_empty()).then_some(nonce);
                match self.hub.db.insert_message(
                    channel,
                    self.user.id,
                    &content,
                    nonce.as_deref(),
                    &attachments,
                ) {
                    Ok(message) => {
                        // The author gets the copy carrying their nonce, so it can
                        // replace the optimistic row they already drew; everyone else
                        // gets it without.
                        let mut public = message.clone();
                        public.nonce = None;
                        self.hub.broadcast_except(self.conn, ServerMsg::MessageCreate(public));
                        self.send(ServerMsg::MessageCreate(message));
                    }
                    Err(err) => self.internal("storing a message", err),
                }
            }

            ClientMsg::EditMessage { id, content } => {
                let content = content.trim().to_string();
                if content.is_empty() || content.chars().count() > MAX_CONTENT {
                    self.fail(ErrorCode::BadRequest, "an edit needs between 1 and 4000 characters");
                    return ControlFlow::Continue(());
                }
                match self.hub.db.edit_message(id, self.user.id, &content) {
                    Ok(Some(message)) => self.hub.broadcast(ServerMsg::MessageUpdate(message)),
                    Ok(None) => self.fail(ErrorCode::Forbidden, "that is not your message"),
                    Err(err) => self.internal("editing a message", err),
                }
            }

            ClientMsg::DeleteMessage { id } => match self.hub.db.delete_message(id, self.user.id) {
                Ok(Some(channel)) => self.hub.broadcast(ServerMsg::MessageDelete { channel, id }),
                Ok(None) => self.fail(ErrorCode::Forbidden, "that is not your message"),
                Err(err) => self.internal("deleting a message", err),
            },

            ClientMsg::Typing { channel } => {
                let now = Instant::now();
                let fresh = self
                    .last_typing
                    .get(&channel)
                    .is_none_or(|last| now.duration_since(*last) >= TYPING_INTERVAL);
                if fresh {
                    self.last_typing.insert(channel, now);
                    self.hub
                        .broadcast_except(self.conn, ServerMsg::Typing { channel, user: self.user.id });
                }
            }

            ClientMsg::CreateChannel { name, kind } => {
                let name = normalise_channel_name(&name);
                if name.is_empty() {
                    self.fail(ErrorCode::BadRequest, "a channel needs a name");
                    return ControlFlow::Continue(());
                }
                match self.hub.db.create_channel(&name, kind) {
                    Ok(channel) => self.hub.broadcast(ServerMsg::ChannelCreate(channel)),
                    Err(err) => self.internal("creating a channel", err),
                }
            }

            ClientMsg::SetDisplayName { name } => {
                let name: String = name.trim().chars().take(MAX_DISPLAY_NAME).collect();
                if let Err(err) = self.hub.db.set_display_name(self.user.id, &name) {
                    self.internal("setting a display name", err);
                    return ControlFlow::Continue(());
                }
                self.user.display_name = name;
                let mut updated = self.user.clone();
                updated.online = true;
                self.hub.broadcast(ServerMsg::UserUpdate(updated));
            }

            ClientMsg::JoinVoice { channel } => {
                match self.hub.db.channel(channel) {
                    Ok(Some(c)) if c.kind == ChannelKind::Voice => {}
                    Ok(Some(_)) => {
                        self.fail(ErrorCode::BadRequest, "that is a text channel");
                        return ControlFlow::Continue(());
                    }
                    Ok(None) => {
                        self.fail(ErrorCode::NotFound, "no such channel");
                        return ControlFlow::Continue(());
                    }
                    Err(err) => {
                        self.internal("checking a channel", err);
                        return ControlFlow::Continue(());
                    }
                }

                let (state, key, previous) = self.hub.join_voice(self.user.id, channel);
                if let Some(previous) = previous {
                    self.hub.broadcast(ServerMsg::VoiceLeave { user: self.user.id, channel: previous });
                }
                // The credentials go only to the joiner; the membership goes to
                // everybody, including the joiner, so one code path draws the roster.
                self.send(ServerMsg::VoiceReady {
                    channel,
                    ssrc: state.ssrc,
                    key,
                    media_port: self.hub.config.media_port,
                });
                self.hub.broadcast(ServerMsg::VoiceState(state));
            }

            ClientMsg::LeaveVoice => {
                let sharing = self.hub.stop_screen(self.user.id).is_some();
                if let Some(channel) = self.hub.leave_voice(self.user.id) {
                    if sharing {
                        self.hub.broadcast(ServerMsg::ScreenStop { user: self.user.id });
                    }
                    self.hub.broadcast(ServerMsg::VoiceLeave { user: self.user.id, channel });
                }
            }

            ClientMsg::UpdateVoiceState { muted, deafened } => {
                match self.hub.set_voice_flags(self.user.id, muted, deafened) {
                    Some(state) => self.hub.broadcast(ServerMsg::VoiceState(state)),
                    None => self.fail(ErrorCode::BadRequest, "you are not in a voice channel"),
                }
            }

            ClientMsg::Speaking { speaking } => {
                // Only to the people who can hear it. Everybody else has no use for a
                // hundred of these a minute.
                for peer in self.hub.voice_peers(self.user.id) {
                    self.hub
                        .send_to_user(peer, ServerMsg::Speaking { user: self.user.id, speaking });
                }
            }

            ClientMsg::StartScreen(request) => {
                if let Err(message) = check_screen_request(&request) {
                    self.fail(ErrorCode::BadRequest, message);
                    return ControlFlow::Continue(());
                }
                match self.hub.start_screen(self.user.id, request) {
                    Some(share) => {
                        self.hub.broadcast(ServerMsg::ScreenStart { user: self.user.id, share })
                    }
                    None => self.fail(ErrorCode::BadRequest, "join a voice channel first"),
                }
            }

            ClientMsg::StopScreen => {
                if self.hub.stop_screen(self.user.id).is_some() {
                    self.hub.broadcast(ServerMsg::ScreenStop { user: self.user.id });
                }
            }

            ClientMsg::WatchScreen { user } => {
                if !self.hub.watch(self.user.id, user) {
                    self.fail(ErrorCode::NotFound, "they are not in your voice channel");
                }
            }

            ClientMsg::UnwatchScreen { user } => self.hub.unwatch(self.user.id, user),

            ClientMsg::OfferFile { to, offer } => {
                // The server relays the offer and has nothing to do with the transfer.
                // The only check worth making is that the recipient exists and is
                // reachable — a code sent into the void would leave the sender waiting
                // on a wormhole nobody will ever claim.
                if !self.hub.send_to_user(to, ServerMsg::FileOffer { from: self.user.id, offer }) {
                    self.fail(ErrorCode::NotFound, "they are not online");
                }
            }

            ClientMsg::CancelFileOffer { to, code } => {
                self.hub
                    .send_to_user(to, ServerMsg::FileOfferCancelled { from: self.user.id, code });
            }
        }
        ControlFlow::Continue(())
    }
}

/// Tidy a channel name: no leading `#`, no whitespace runs, a length limit.
fn normalise_channel_name(name: &str) -> String {
    let name = name.trim().trim_start_matches('#');
    name.split_whitespace().collect::<Vec<_>>().join("-").chars().take(48).collect()
}

/// Sanity-check a screen share request.
///
/// Note what is *not* checked: there is no ceiling on bitrate, resolution or frame
/// rate. That is the point of the project — quality is a property of the machine
/// doing the encoding and the uplink it has, not a tier somebody sells. The limits
/// here only reject values that cannot describe a real capture, because those would
/// otherwise reach a decoder as a promise it cannot keep.
fn check_screen_request(request: &boa_proto::control::ScreenRequest) -> Result<(), String> {
    if request.width == 0 || request.height == 0 {
        return Err("a share needs a size".into());
    }
    // 16K, which is not a quality judgement: H.264 level limits and every decoder in
    // existence stop well before this, and a width of four billion is a mistake
    // rather than an ambition.
    if request.width > 16_384 || request.height > 16_384 {
        return Err("that is not a screen size".into());
    }
    if request.fps == 0 || request.fps > 240 {
        return Err("frame rate must be between 1 and 240".into());
    }
    if request.kbps == 0 {
        return Err("bitrate must be greater than zero".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_proto::control::ScreenRequest;

    #[test]
    fn channel_names_are_tidied_not_rejected() {
        assert_eq!(normalise_channel_name("  #General  "), "General");
        assert_eq!(normalise_channel_name("two   words"), "two-words");
        assert_eq!(normalise_channel_name(""), "");
        assert_eq!(normalise_channel_name("###"), "");
        assert_eq!(normalise_channel_name(&"x".repeat(100)).chars().count(), 48);
    }

    #[test]
    fn a_screen_request_is_checked_for_sense_not_for_quality() {
        let sane = ScreenRequest { width: 1920, height: 1080, fps: 60, kbps: 12_000, with_audio: true };
        assert!(check_screen_request(&sane).is_ok());

        // The whole point: no ceiling on quality. 4K at 120 with a 100 Mbit/s target
        // is somebody's LAN, and it is allowed.
        let extravagant = ScreenRequest { width: 3840, height: 2160, fps: 120, kbps: 100_000, with_audio: true };
        assert!(check_screen_request(&extravagant).is_ok());

        assert!(check_screen_request(&ScreenRequest { width: 0, ..sane }).is_err());
        assert!(check_screen_request(&ScreenRequest { height: 0, ..sane }).is_err());
        assert!(check_screen_request(&ScreenRequest { fps: 0, ..sane }).is_err());
        assert!(check_screen_request(&ScreenRequest { fps: 1_000, ..sane }).is_err());
        assert!(check_screen_request(&ScreenRequest { kbps: 0, ..sane }).is_err());
        assert!(check_screen_request(&ScreenRequest { width: 99_999, ..sane }).is_err());
    }
}
