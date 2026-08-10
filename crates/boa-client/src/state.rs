//! What the client believes about the server, and the one function that changes it.
//!
//! Every field here is derived from [`ServerMsg`]s, and [`State::apply`] is the only place
//! that touches them. Nothing in the interface writes to this struct — a click produces a
//! [`ClientMsg`], the server answers, and the answer lands here. That is a round trip for
//! things like "muted", and it is deliberate: the alternative is optimistic local state
//! that can disagree with the server, and a mute button that *looks* engaged while the
//! microphone is live is the worst bug this app could have.
//!
//! The one exception is [`State::pending`], where a sent message is drawn before the server
//! has confirmed it — because a chat window that does not show what you just typed until a
//! round trip completes feels broken on any connection worse than a LAN. It is a separate
//! list rather than a fake entry in the log, so nothing else in the app has to know that
//! some messages are not real yet.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use boa_proto::{
    Channel, ChannelKind, FileOffer, Id, Message, ScreenShare, ServerInfo, ServerMsg, User,
    VoiceState,
};

/// How long a typing indicator stays up without being refreshed.
///
/// The server throttles each client's notices to one every three seconds, so this has to be
/// longer than that or the indicator would flicker between them.
pub const TYPING_TTL: Duration = Duration::from_secs(5);

/// How long a name stays lit after the last `Speaking { speaking: true }`.
///
/// A safety net, not the mechanism: the sender also announces when it *stops*. This covers
/// the case where that message is lost or the sender disappears mid-word, which otherwise
/// leaves somebody permanently marked as talking.
pub const SPEAKING_TTL: Duration = Duration::from_secs(2);

/// One channel's messages, and how much of its past we have.
#[derive(Default)]
pub struct ChannelLog {
    /// Oldest first, unique by id.
    pub messages: Vec<Message>,
    /// Set when a history page reached the beginning of the channel, so the scroller stops
    /// asking rather than discovering it by getting an empty page every time.
    pub complete: bool,
    /// A history request is outstanding. Guards against firing one per frame while the
    /// scroll is held at the top.
    pub loading: bool,
    /// Whether this channel has ever been opened, so the first look fetches a page.
    pub visited: bool,
}

/// A message this client has sent and the server has not confirmed.
pub struct Pending {
    pub nonce: String,
    pub channel: Id,
    pub content: String,
    pub attachment_names: Vec<String>,
    pub sent_at: Instant,
}

impl Pending {
    /// Whether this has been waiting long enough to look stuck.
    ///
    /// Five seconds. Not a timeout — the message may still land — but long enough that the
    /// interface should stop pretending everything is fine and grey it out.
    pub fn is_slow(&self) -> bool {
        self.sent_at.elapsed() > Duration::from_secs(5)
    }
}

#[derive(Default)]
pub struct State {
    /// Who we are, once `Ready` has arrived.
    pub me: Option<User>,
    pub server: Option<ServerInfo>,
    pub users: BTreeMap<Id, User>,
    pub channels: Vec<Channel>,
    pub logs: HashMap<Id, ChannelLog>,
    pub pending: Vec<Pending>,
    /// Voice membership, by user.
    pub voice: HashMap<Id, VoiceState>,
    /// When each person was last heard from, for the speaking ring.
    speaking: HashMap<Id, Instant>,
    /// When each person was last seen typing in each channel.
    typing: HashMap<(Id, Id), Instant>,
    /// Offers of a direct file transfer, newest last.
    pub offers: Vec<(Id, FileOffer)>,
}

impl State {
    /// Throw everything away. Called on disconnect: stale membership and a stale roster are
    /// worse than an empty window, because they look current.
    pub fn reset(&mut self) {
        *self = State::default();
    }

    pub fn user(&self, id: Id) -> Option<&User> {
        self.users.get(&id)
    }

    /// The name to draw for a user id, even one we have never heard of.
    ///
    /// A message from an account that was deleted, or one that arrived before its `Ready`,
    /// must still render. `#7` is deliberately odd-looking: it is a fact about the data
    /// rather than a plausible name somebody might mistake for real.
    pub fn label(&self, id: Id) -> String {
        self.users.get(&id).map(|u| u.label().to_string()).unwrap_or_else(|| format!("#{id}"))
    }

    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn log(&self, channel: Id) -> Option<&ChannelLog> {
        self.logs.get(&channel)
    }

    pub fn log_mut(&mut self, channel: Id) -> &mut ChannelLog {
        self.logs.entry(channel).or_default()
    }

    /// The first text channel, for deciding what to open on a fresh start.
    pub fn first_text_channel(&self) -> Option<Id> {
        self.channels.iter().find(|c| c.kind == ChannelKind::Text).map(|c| c.id)
    }

    /// Our own voice state, if we are in a call.
    pub fn my_voice(&self) -> Option<&VoiceState> {
        self.me.as_ref().and_then(|me| self.voice.get(&me.id))
    }

    pub fn my_channel(&self) -> Option<Id> {
        self.my_voice().map(|state| state.channel)
    }

    /// Everybody in a voice channel, in a stable order.
    pub fn voice_members(&self, channel: Id) -> Vec<VoiceState> {
        let mut members: Vec<VoiceState> =
            self.voice.values().filter(|state| state.channel == channel).copied().collect();
        // By name rather than by id, because the roster is read by people. Ties break on id
        // so the order cannot flicker between frames.
        members.sort_by(|a, b| {
            self.label(a.user).to_lowercase().cmp(&self.label(b.user).to_lowercase()).then(a.user.cmp(&b.user))
        });
        members
    }

    /// Whether this person's name should be lit up.
    pub fn is_speaking(&self, user: Id) -> bool {
        self.speaking.get(&user).is_some_and(|at| at.elapsed() < SPEAKING_TTL)
    }

    /// Anybody sharing their screen in this channel.
    pub fn sharers(&self, channel: Id) -> Vec<(Id, ScreenShare)> {
        let mut sharing: Vec<(Id, ScreenShare)> = self
            .voice
            .values()
            .filter(|state| state.channel == channel)
            .filter_map(|state| state.screen.map(|share| (state.user, share)))
            .collect();
        sharing.sort_by_key(|(user, _)| *user);
        sharing
    }

    /// Who is typing in this channel right now, excluding ourselves.
    pub fn typers(&self, channel: Id) -> Vec<Id> {
        let me = self.me.as_ref().map(|me| me.id);
        let mut typers: Vec<Id> = self
            .typing
            .iter()
            .filter(|((c, user), at)| {
                *c == channel && Some(*user) != me && at.elapsed() < TYPING_TTL
            })
            .map(|((_, user), _)| *user)
            .collect();
        typers.sort();
        typers
    }

    /// Drop indicators whose time is up.
    ///
    /// Called once per frame rather than checked at every read, so the maps do not grow for
    /// the life of the session with an entry per person per channel.
    pub fn expire(&mut self) {
        self.typing.retain(|_, at| at.elapsed() < TYPING_TTL);
        self.speaking.retain(|_, at| at.elapsed() < SPEAKING_TTL);
    }

    /// Note a message we have just sent, so it can be drawn before the server answers.
    pub fn add_pending(&mut self, pending: Pending) {
        self.pending.push(pending);
    }

    /// Pending messages for a channel.
    pub fn pending_for(&self, channel: Id) -> impl Iterator<Item = &Pending> {
        self.pending.iter().filter(move |p| p.channel == channel)
    }

    /// Apply one frame from the server.
    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Ready { user, server, users, channels, voice_states } => {
                // A `Ready` replaces everything rather than merging, because it arrives
                // after a reconnect too and anything kept from before is a guess about a
                // gap we did not see.
                let logs = std::mem::take(&mut self.logs);
                let pending = std::mem::take(&mut self.pending);
                *self = State::default();
                self.me = Some(user);
                self.server = Some(server);
                self.users = users.into_iter().map(|u| (u.id, u)).collect();
                self.channels = channels;
                self.voice = voice_states.into_iter().map(|s| (s.user, s)).collect();
                // Messages and unconfirmed sends *are* kept: they are what the user was
                // reading and typing, and a reconnect that empties the window loses their
                // place for no reason. Anything missed comes back from history.
                self.logs = logs;
                self.pending = pending;
            }

            ServerMsg::MessageCreate(message) => {
                // Our own message coming back: drop the optimistic copy first, or the
                // window shows it twice for one frame.
                if let Some(nonce) = &message.nonce {
                    self.pending.retain(|p| &p.nonce != nonce);
                }
                self.insert(message);
            }
            ServerMsg::MessageUpdate(message) => {
                let log = self.log_mut(message.channel);
                match log.messages.binary_search_by_key(&message.id, |m| m.id) {
                    Ok(at) => log.messages[at] = message,
                    // An edit of a message we never had. Inserted rather than dropped: it is
                    // the current text of a real message, and dropping it means a gap that
                    // only a reload would fill.
                    Err(at) => log.messages.insert(at, message),
                }
            }
            ServerMsg::MessageDelete { channel, id } => {
                self.log_mut(channel).messages.retain(|m| m.id != id);
            }
            ServerMsg::History { channel, messages, complete } => {
                let log = self.log_mut(channel);
                log.loading = false;
                log.complete = log.complete || complete;
                for message in messages {
                    insert_into(&mut log.messages, message);
                }
            }

            ServerMsg::Typing { channel, user } => {
                self.typing.insert((channel, user), Instant::now());
            }
            ServerMsg::Presence { user, online } => {
                if let Some(known) = self.users.get_mut(&user) {
                    known.online = online;
                }
            }
            ServerMsg::UserUpdate(user) => {
                self.users.insert(user.id, user);
            }
            ServerMsg::ChannelCreate(channel) => {
                if !self.channels.iter().any(|c| c.id == channel.id) {
                    self.channels.push(channel);
                    // Kept in the server's own order — by kind, then position, then id — so
                    // everybody's sidebar looks the same.
                    self.channels.sort_by(|a, b| {
                        kind_order(a.kind)
                            .cmp(&kind_order(b.kind))
                            .then(a.position.cmp(&b.position))
                            .then(a.id.cmp(&b.id))
                    });
                }
            }

            ServerMsg::VoiceState(state) => {
                self.voice.insert(state.user, state);
            }
            ServerMsg::VoiceLeave { user, channel } => {
                // Only if they are still recorded in *that* channel: a leave for a channel
                // somebody has already moved out of is a stale frame, and honouring it would
                // remove them from the call they are actually in.
                if self.voice.get(&user).is_some_and(|state| state.channel == channel) {
                    self.voice.remove(&user);
                }
                self.speaking.remove(&user);
            }
            ServerMsg::Speaking { user, speaking } => {
                if speaking {
                    self.speaking.insert(user, Instant::now());
                } else {
                    self.speaking.remove(&user);
                }
            }
            ServerMsg::ScreenStart { user, share } => {
                if let Some(state) = self.voice.get_mut(&user) {
                    state.screen = Some(share);
                }
            }
            ServerMsg::ScreenStop { user } => {
                if let Some(state) = self.voice.get_mut(&user) {
                    state.screen = None;
                }
            }

            ServerMsg::FileOffer { from, offer } => {
                // One offer per code. A resend after a reconnect is the same offer.
                self.offers.retain(|(_, existing)| existing.code != offer.code);
                self.offers.push((from, offer));
            }
            ServerMsg::FileOfferCancelled { from, code } => {
                self.offers.retain(|(who, offer)| !(*who == from && offer.code == code));
            }

            // Handled by the interface, not stored: the voice credentials go to the audio
            // engine, an attachment record goes to whatever asked for it, errors go to the
            // status area, and a pong is only interesting for its round trip.
            ServerMsg::VoiceReady { .. }
            | ServerMsg::AttachmentReady(_)
            | ServerMsg::Error { .. }
            | ServerMsg::Pong { .. } => {}
        }
    }

    fn insert(&mut self, message: Message) {
        let log = self.logs.entry(message.channel).or_default();
        insert_into(&mut log.messages, message);
    }
}

/// Put a message in its place, replacing any existing copy with the same id.
///
/// Ids are monotonic, so the list stays sorted by id and a binary search finds the slot.
/// Worth doing properly rather than pushing and sorting: history pages, live messages and a
/// reconnect's refill all land here, in no particular order relative to each other, and a
/// duplicate would show up as the same line twice.
fn insert_into(messages: &mut Vec<Message>, message: Message) {
    match messages.binary_search_by_key(&message.id, |m| m.id) {
        Ok(at) => messages[at] = message,
        Err(at) => messages.insert(at, message),
    }
}

/// Text channels above voice channels, as in the sidebar.
fn kind_order(kind: ChannelKind) -> u8 {
    match kind {
        ChannelKind::Text => 0,
        ChannelKind::Voice => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u64, name: &str) -> User {
        User { id: Id(id), name: name.into(), display_name: name.into(), online: true }
    }

    fn channel(id: u64, name: &str, kind: ChannelKind) -> Channel {
        Channel { id: Id(id), name: name.into(), kind, position: 0, topic: String::new() }
    }

    fn message(id: u64, channel: u64, author: u64, content: &str) -> Message {
        Message {
            id: Id(id),
            channel: Id(channel),
            author: Id(author),
            content: content.into(),
            created_at: id as i64,
            edited_at: None,
            attachments: vec![],
            nonce: None,
        }
    }

    fn ready() -> State {
        let mut state = State::default();
        state.apply(ServerMsg::Ready {
            user: user(1, "ada"),
            server: ServerInfo {
                name: "Home".into(),
                media_port: 8788,
                protocol_version: boa_proto::PROTOCOL_VERSION,
                attachment_ttl_secs: boa_proto::ATTACHMENT_TTL_SECS,
                max_upload_bytes: 1 << 20,
                wormhole_rendezvous: None,
                wormhole_transit: None,
            },
            users: vec![user(1, "ada"), user(2, "bob")],
            channels: vec![channel(10, "general", ChannelKind::Text), channel(11, "Lounge", ChannelKind::Voice)],
            voice_states: vec![],
        });
        state
    }

    #[test]
    fn ready_populates_everything_the_window_needs() {
        let state = ready();
        assert_eq!(state.me.as_ref().unwrap().name, "ada");
        assert_eq!(state.users.len(), 2);
        assert_eq!(state.first_text_channel(), Some(Id(10)));
        assert_eq!(state.label(Id(2)), "bob");
        // An id nobody knows still renders, and visibly as an id.
        assert_eq!(state.label(Id(99)), "#99");
    }

    /// The three sources of messages — live, history and a reconnect's refill — all land in
    /// one list, in no particular order relative to each other.
    #[test]
    fn messages_stay_sorted_and_unique_whatever_order_they_arrive_in() {
        let mut state = ready();
        state.apply(ServerMsg::MessageCreate(message(5, 10, 1, "five")));
        state.apply(ServerMsg::MessageCreate(message(3, 10, 1, "three")));
        state.apply(ServerMsg::History {
            channel: Id(10),
            messages: vec![message(1, 10, 2, "one"), message(3, 10, 1, "three again"), message(4, 10, 2, "four")],
            complete: true,
        });

        let log = state.log(Id(10)).unwrap();
        assert_eq!(log.messages.iter().map(|m| m.id.0).collect::<Vec<_>>(), vec![1, 3, 4, 5]);
        assert_eq!(log.messages[1].content, "three again", "the later copy wins");
        assert!(log.complete);
        assert!(!log.loading);
    }

    /// The optimistic-send path: what was typed appears at once, and does not appear twice
    /// when the server's copy arrives.
    #[test]
    fn a_pending_message_is_replaced_by_the_servers_copy() {
        let mut state = ready();
        state.add_pending(Pending {
            nonce: "n1".into(),
            channel: Id(10),
            content: "hello".into(),
            attachment_names: vec![],
            sent_at: Instant::now(),
        });
        assert_eq!(state.pending_for(Id(10)).count(), 1);

        let mut confirmed = message(7, 10, 1, "hello");
        confirmed.nonce = Some("n1".into());
        state.apply(ServerMsg::MessageCreate(confirmed));

        assert_eq!(state.pending_for(Id(10)).count(), 0, "the optimistic copy is gone");
        assert_eq!(state.log(Id(10)).unwrap().messages.len(), 1);

        // Somebody else's message has no nonce and must not clear anything.
        state.add_pending(Pending {
            nonce: "n2".into(),
            channel: Id(10),
            content: "again".into(),
            attachment_names: vec![],
            sent_at: Instant::now(),
        });
        state.apply(ServerMsg::MessageCreate(message(8, 10, 2, "hi")));
        assert_eq!(state.pending_for(Id(10)).count(), 1);
    }

    #[test]
    fn an_edit_of_an_unseen_message_is_kept_rather_than_dropped() {
        let mut state = ready();
        let mut edited = message(4, 10, 1, "revised");
        edited.edited_at = Some(99);
        state.apply(ServerMsg::MessageUpdate(edited));
        assert_eq!(state.log(Id(10)).unwrap().messages.len(), 1);

        state.apply(ServerMsg::MessageDelete { channel: Id(10), id: Id(4) });
        assert!(state.log(Id(10)).unwrap().messages.is_empty());
        // Deleting something that was never there is not an error.
        state.apply(ServerMsg::MessageDelete { channel: Id(10), id: Id(4) });
    }

    #[test]
    fn voice_membership_follows_join_and_leave() {
        let mut state = ready();
        let ada = VoiceState { user: Id(1), channel: Id(11), muted: false, deafened: false, ssrc: 1, screen: None };
        let bob = VoiceState { user: Id(2), channel: Id(11), muted: true, deafened: false, ssrc: 3, screen: None };
        state.apply(ServerMsg::VoiceState(ada));
        state.apply(ServerMsg::VoiceState(bob));

        assert_eq!(state.my_channel(), Some(Id(11)));
        let members = state.voice_members(Id(11));
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].user, Id(1), "sorted by name: ada before bob");
        assert!(members[1].muted);

        state.apply(ServerMsg::VoiceLeave { user: Id(2), channel: Id(11) });
        assert_eq!(state.voice_members(Id(11)).len(), 1);
    }

    /// A leave for a channel somebody has already left is a stale frame — and honouring it
    /// would take them out of the call they moved to.
    #[test]
    fn a_stale_voice_leave_is_ignored() {
        let mut state = ready();
        state.apply(ServerMsg::VoiceState(VoiceState {
            user: Id(2),
            channel: Id(12),
            muted: false,
            deafened: false,
            ssrc: 5,
            screen: None,
        }));
        state.apply(ServerMsg::VoiceLeave { user: Id(2), channel: Id(11) });
        assert_eq!(state.voice.get(&Id(2)).map(|s| s.channel), Some(Id(12)));
    }

    #[test]
    fn a_screen_share_attaches_to_its_owners_voice_state() {
        let mut state = ready();
        state.apply(ServerMsg::VoiceState(VoiceState {
            user: Id(2),
            channel: Id(11),
            muted: false,
            deafened: false,
            ssrc: 3,
            screen: None,
        }));
        let share = ScreenShare { ssrc: 4, width: 1920, height: 1080, fps: 60, kbps: 8_000, with_audio: true };
        state.apply(ServerMsg::ScreenStart { user: Id(2), share });
        assert_eq!(state.sharers(Id(11)), vec![(Id(2), share)]);

        state.apply(ServerMsg::ScreenStop { user: Id(2) });
        assert!(state.sharers(Id(11)).is_empty());

        // A share for somebody not in a call is dropped rather than inventing a member.
        state.apply(ServerMsg::ScreenStart { user: Id(9), share });
        assert!(state.sharers(Id(11)).is_empty());
    }

    #[test]
    fn speaking_and_typing_both_expire() {
        let mut state = ready();
        state.apply(ServerMsg::Speaking { user: Id(2), speaking: true });
        state.apply(ServerMsg::Typing { channel: Id(10), user: Id(2) });
        assert!(state.is_speaking(Id(2)));
        assert_eq!(state.typers(Id(10)), vec![Id(2)]);

        // Age both by hand rather than sleeping for five seconds.
        let old = Instant::now() - Duration::from_secs(60);
        state.speaking.insert(Id(2), old);
        state.typing.insert((Id(10), Id(2)), old);
        assert!(!state.is_speaking(Id(2)));
        assert!(state.typers(Id(10)).is_empty());

        // And the maps are actually emptied, or they grow for the session.
        state.expire();
        assert!(state.speaking.is_empty());
        assert!(state.typing.is_empty());
    }

    #[test]
    fn an_explicit_stop_clears_speaking_at_once() {
        let mut state = ready();
        state.apply(ServerMsg::Speaking { user: Id(2), speaking: true });
        state.apply(ServerMsg::Speaking { user: Id(2), speaking: false });
        assert!(!state.is_speaking(Id(2)));
    }

    #[test]
    fn we_never_show_ourselves_as_typing() {
        let mut state = ready();
        state.apply(ServerMsg::Typing { channel: Id(10), user: Id(1) });
        assert!(state.typers(Id(10)).is_empty());
    }

    /// A reconnect must not lose what somebody was reading, and must not keep a roster that
    /// might have changed while we were away.
    #[test]
    fn a_second_ready_refreshes_the_roster_and_keeps_the_messages() {
        let mut state = ready();
        state.apply(ServerMsg::MessageCreate(message(5, 10, 1, "before the drop")));
        state.add_pending(Pending {
            nonce: "n".into(),
            channel: Id(10),
            content: "unsent".into(),
            attachment_names: vec![],
            sent_at: Instant::now(),
        });
        state.apply(ServerMsg::VoiceState(VoiceState {
            user: Id(2),
            channel: Id(11),
            muted: false,
            deafened: false,
            ssrc: 3,
            screen: None,
        }));

        state.apply(ServerMsg::Ready {
            user: user(1, "ada"),
            server: state.server.clone().unwrap(),
            users: vec![user(1, "ada")],
            channels: vec![channel(10, "general", ChannelKind::Text)],
            voice_states: vec![],
        });

        assert_eq!(state.log(Id(10)).unwrap().messages.len(), 1, "the log survives");
        assert_eq!(state.pending.len(), 1, "so does an unconfirmed send");
        assert_eq!(state.users.len(), 1, "but the roster is the server's, not ours");
        assert!(state.voice.is_empty(), "and stale voice membership is dropped");
    }

    #[test]
    fn new_channels_land_in_the_servers_order() {
        let mut state = ready();
        state.apply(ServerMsg::ChannelCreate(channel(12, "random", ChannelKind::Text)));
        state.apply(ServerMsg::ChannelCreate(channel(13, "Music", ChannelKind::Voice)));
        // Text channels first, voice after, whatever order they arrived in.
        let kinds: Vec<ChannelKind> = state.channels.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![ChannelKind::Text, ChannelKind::Text, ChannelKind::Voice, ChannelKind::Voice]
        );
        // And a duplicate announcement does not double the entry.
        state.apply(ServerMsg::ChannelCreate(channel(12, "random", ChannelKind::Text)));
        assert_eq!(state.channels.len(), 4);
    }

    #[test]
    fn a_file_offer_is_replaced_rather_than_stacked_and_can_be_withdrawn() {
        let mut state = ready();
        let offer = FileOffer { code: "7-crossover-clockwork".into(), name: "a.zip".into(), size: 10, channel: Id(10) };
        state.apply(ServerMsg::FileOffer { from: Id(2), offer: offer.clone() });
        state.apply(ServerMsg::FileOffer { from: Id(2), offer: offer.clone() });
        assert_eq!(state.offers.len(), 1, "a resend after a reconnect is the same offer");

        state.apply(ServerMsg::FileOfferCancelled { from: Id(2), code: offer.code.clone() });
        assert!(state.offers.is_empty());
    }

    #[test]
    fn disconnecting_forgets_everything() {
        let mut state = ready();
        state.apply(ServerMsg::MessageCreate(message(5, 10, 1, "hi")));
        state.reset();
        assert!(state.me.is_none());
        assert!(state.channels.is_empty());
        assert!(state.logs.is_empty());
    }
}
