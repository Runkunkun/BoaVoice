//! Everything the server knows that does not survive a restart: who is connected,
//! who is in which voice channel, and who is watching whose screen.
//!
//! One mutex over one struct. That is a defensible choice at this size and a bad one
//! at Discord's, so it is worth saying why it holds here: every operation below is a
//! few hash lookups and a `Vec` of message clones, the lock is never held across an
//! `await` (all sends are into unbounded channels, which never block), and a
//! self-hosted server's contention is a dozen people rather than a million. Sharding
//! this would add a class of bug — two locks, two orders — in exchange for
//! throughput nobody here needs.
//!
//! The rule that keeps it honest: **the hub never awaits.** Every method returns
//! quickly with owned data, so a caller cannot hold the lock while doing I/O.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use boa_proto::{
    Id, MediaKind, PacketHeader, ScreenShare, ServerMsg, SessionKey, User, VoiceState,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::blobs::Blobs;
use crate::config::Config;
use crate::db::Db;

/// Identifies one control connection.
///
/// Not the user id: the same account can be signed in from a laptop and a phone, and
/// both connections have to be addressable separately — otherwise closing one would
/// mark the account offline while the other is still in a call.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ConnId(pub u64);

/// What the relay should do with a datagram.
#[derive(Debug)]
pub enum Route {
    /// A registration packet. Verify it against `key`, and if it is genuine call
    /// [`Hub::bind_media_address`].
    Register { key: SessionKey, user: Id },
    /// Forward the datagram, unmodified, to these addresses.
    Forward(Vec<SocketAddr>),
    /// Not from a known stream, or from the wrong address. Dropped without a reply:
    /// answering an unknown sender turns the relay into a reflector somebody else
    /// can point at a third party.
    Drop,
}

struct Conn {
    user: Id,
    tx: UnboundedSender<ServerMsg>,
}

/// One person's presence in a voice channel.
struct Member {
    channel: Id,
    /// The stream id for their microphone.
    voice_ssrc: u32,
    /// The stream id their screen share would use. Allocated at join rather than at
    /// share time, so the number is stable for the whole session and a viewer's
    /// subscription cannot be aimed at a stale one.
    screen_ssrc: u32,
    muted: bool,
    deafened: bool,
    screen: Option<ScreenShare>,
    /// Whose screens this member is watching.
    watching: HashSet<Id>,
    /// Learned from a registration packet, not from the control connection: the
    /// source address of the TCP socket is very often not the address UDP will
    /// arrive from, because NAT assigns per-protocol mappings.
    addr: Option<SocketAddr>,
    last_seen: Instant,
}

struct State {
    conns: HashMap<ConnId, Conn>,
    members: HashMap<Id, Member>,
    by_ssrc: HashMap<u32, Id>,
    /// One key per voice channel, created when the channel becomes occupied and
    /// dropped when it empties. Shared by everyone in the channel — transport
    /// encryption, not end-to-end; see [`boa_proto::SessionKey`].
    keys: HashMap<Id, SessionKey>,
    next_conn: u64,
    next_ssrc: u32,
}

pub struct Hub {
    pub config: Config,
    pub db: Arc<Db>,
    pub blobs: Arc<Blobs>,
    /// What the relay has forwarded and refused. Held here so the HTTP side can publish it: three
    /// numbers turn "the screen share is stuttering" from a guess into a measurement.
    pub stats: Arc<crate::relay::Stats>,
    /// When this process started, which is the denominator for those numbers.
    pub started: std::time::Instant,
    state: Mutex<State>,
}

impl Hub {
    /// A hub with counters of its own, for tests that do not run a relay.
    #[cfg(test)]
    pub fn new(config: Config, db: Arc<Db>, blobs: Arc<Blobs>) -> Self {
        Hub::with_stats(config, db, blobs, Arc::new(crate::relay::Stats::default()))
    }

    /// Sharing the relay's counters — which is how the real server builds it, so that `/api/stats` and
    /// the relay are looking at one set of numbers rather than two.
    pub fn with_stats(
        config: Config,
        db: Arc<Db>,
        blobs: Arc<Blobs>,
        stats: Arc<crate::relay::Stats>,
    ) -> Self {
        Hub {
            config,
            db,
            blobs,
            stats,
            started: std::time::Instant::now(),
            state: Mutex::new(State {
                conns: HashMap::new(),
                members: HashMap::new(),
                by_ssrc: HashMap::new(),
                keys: HashMap::new(),
                next_conn: 1,
                // Stream ids start at 1 so that zero can mean "none" in a log line
                // without ambiguity.
                next_ssrc: 1,
            }),
        }
    }

    /// The state mutex, unpoisoned — see [`crate::db::Db`] for the same reasoning:
    /// one panicking request should not take the server's voice channels with it.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            log::error!("hub mutex was poisoned by a panic; carrying on");
            poisoned.into_inner()
        })
    }

    // ----------------------------------------------------------------------- //
    // Connections
    // ----------------------------------------------------------------------- //

    /// Register a connection. Returns its id and whether this account was offline
    /// until now (so the caller knows whether to announce presence).
    pub fn connect(&self, user: Id, tx: UnboundedSender<ServerMsg>) -> (ConnId, bool) {
        let mut state = self.state();
        let first = !state.conns.values().any(|c| c.user == user);
        let id = ConnId(state.next_conn);
        state.next_conn += 1;
        state.conns.insert(id, Conn { user, tx });
        (id, first)
    }

    /// Remove a connection. Returns whether that was the account's last one.
    pub fn disconnect(&self, conn: ConnId) -> bool {
        let mut state = self.state();
        let Some(gone) = state.conns.remove(&conn) else { return false };
        !state.conns.values().any(|c| c.user == gone.user)
    }

    pub fn online(&self) -> HashSet<Id> {
        self.state().conns.values().map(|c| c.user).collect()
    }

    /// Send to every connection of every user.
    pub fn broadcast(&self, msg: ServerMsg) {
        let state = self.state();
        for conn in state.conns.values() {
            // A closed channel means that connection's writer task has already gone
            // away; its entry is removed by `disconnect` on the way out, so a failure
            // here is expected rather than an error.
            let _ = conn.tx.send(msg.clone());
        }
    }

    /// Send to every connection except one — the sender's own, for events it has
    /// already applied locally.
    pub fn broadcast_except(&self, except: ConnId, msg: ServerMsg) {
        let state = self.state();
        for (id, conn) in &state.conns {
            if *id != except {
                let _ = conn.tx.send(msg.clone());
            }
        }
    }

    /// Send to every connection of one account.
    pub fn send_to_user(&self, user: Id, msg: ServerMsg) -> bool {
        let state = self.state();
        let mut delivered = false;
        for conn in state.conns.values().filter(|c| c.user == user) {
            delivered |= conn.tx.send(msg.clone()).is_ok();
        }
        delivered
    }

    pub fn send_to_conn(&self, conn: ConnId, msg: ServerMsg) {
        let state = self.state();
        if let Some(conn) = state.conns.get(&conn) {
            let _ = conn.tx.send(msg);
        }
    }

    /// Every user marked with their live presence, for `Ready`.
    pub fn decorate_presence(&self, users: &mut [User]) {
        let online = self.online();
        for user in users {
            user.online = online.contains(&user.id);
        }
    }

    // ----------------------------------------------------------------------- //
    // Voice membership
    // ----------------------------------------------------------------------- //

    /// Join a voice channel, leaving any previous one.
    ///
    /// Returns the new state and the channel that was left, if any — the caller has
    /// to announce both, and doing the leave here rather than asking the caller to
    /// remember means a client that sends two joins cannot end up listed in two
    /// channels at once.
    pub fn join_voice(&self, user: Id, channel: Id) -> (VoiceState, String, Option<Id>) {
        let mut state = self.state();
        let previous = state.remove_member(user);

        let voice_ssrc = state.take_ssrc();
        let screen_ssrc = state.take_ssrc();
        state.by_ssrc.insert(voice_ssrc, user);
        state.by_ssrc.insert(screen_ssrc, user);

        let key = state
            .keys
            .entry(channel)
            .or_insert_with(SessionKey::random)
            .clone();

        state.members.insert(
            user,
            Member {
                channel,
                voice_ssrc,
                screen_ssrc,
                muted: false,
                deafened: false,
                screen: None,
                watching: HashSet::new(),
                addr: None,
                last_seen: Instant::now(),
            },
        );

        let voice_state = VoiceState {
            user,
            channel,
            muted: false,
            deafened: false,
            ssrc: voice_ssrc,
            screen: None,
        };
        (voice_state, key.to_base64(), previous)
    }

    /// Leave whatever voice channel this user is in. Returns the channel.
    pub fn leave_voice(&self, user: Id) -> Option<Id> {
        self.state().remove_member(user)
    }

    pub fn set_voice_flags(&self, user: Id, muted: bool, deafened: bool) -> Option<VoiceState> {
        let mut state = self.state();
        let member = state.members.get_mut(&user)?;
        member.muted = muted;
        member.deafened = deafened;
        Some(member.state(user))
    }

    /// Begin a screen share. Returns the share with the stream id filled in.
    pub fn start_screen(
        &self,
        user: Id,
        request: boa_proto::control::ScreenRequest,
    ) -> Option<ScreenShare> {
        let mut state = self.state();
        let member = state.members.get_mut(&user)?;
        let share = ScreenShare {
            ssrc: member.screen_ssrc,
            width: request.width,
            height: request.height,
            fps: request.fps,
            kbps: request.kbps,
            with_audio: request.with_audio,
        };
        member.screen = Some(share);
        Some(share)
    }

    /// Stop sharing. Returns the channel to announce it in, and clears everybody's
    /// subscription — a viewer whose subscription outlived the share would keep a
    /// stale entry that silently swallows the *next* share's packets, because the
    /// relay would think they were already watching.
    pub fn stop_screen(&self, user: Id) -> Option<Id> {
        let mut state = self.state();
        let channel = {
            let member = state.members.get_mut(&user)?;
            member.screen = None;
            member.channel
        };
        for member in state.members.values_mut() {
            member.watching.remove(&user);
        }
        Some(channel)
    }

    /// Subscribe to somebody's screen. Both have to be in the same channel.
    pub fn watch(&self, viewer: Id, target: Id) -> bool {
        let mut state = self.state();
        let Some(target_channel) = state.members.get(&target).map(|m| m.channel) else {
            return false;
        };
        match state.members.get_mut(&viewer) {
            Some(member) if member.channel == target_channel => {
                member.watching.insert(target);
                true
            }
            _ => false,
        }
    }

    pub fn unwatch(&self, viewer: Id, target: Id) {
        if let Some(member) = self.state().members.get_mut(&viewer) {
            member.watching.remove(&target);
        }
    }

    /// Whether `viewer` is subscribed to `target`'s screen *and* `target` is sharing one.
    ///
    /// The check in front of forwarding a quality report. Not paranoia about a hostile client so much
    /// as about a stale one: a report can be in flight when a share stops, and forwarding it to
    /// somebody who is no longer sharing would have them adjust an encoder that does not exist.
    pub fn is_watching(&self, viewer: Id, target: Id) -> bool {
        let state = self.state();
        let sharing = state.members.get(&target).is_some_and(|m| m.screen.is_some());
        let subscribed = state.members.get(&viewer).is_some_and(|m| m.watching.contains(&target));
        sharing && subscribed
    }

    pub fn voice_states(&self) -> Vec<VoiceState> {
        let state = self.state();
        let mut states: Vec<VoiceState> =
            state.members.iter().map(|(user, m)| m.state(*user)).collect();
        // Sorted so `Ready` is deterministic, which makes a packet capture or a test
        // comparable between runs.
        states.sort_by_key(|s| s.user);
        states
    }

    pub fn voice_channel_of(&self, user: Id) -> Option<Id> {
        self.state().members.get(&user).map(|m| m.channel)
    }

    /// Who else is in this user's voice channel.
    pub fn voice_peers(&self, user: Id) -> Vec<Id> {
        let state = self.state();
        let Some(channel) = state.members.get(&user).map(|m| m.channel) else {
            return Vec::new();
        };
        state
            .members
            .iter()
            .filter(|(other, m)| **other != user && m.channel == channel)
            .map(|(other, _)| *other)
            .collect()
    }

    // ----------------------------------------------------------------------- //
    // The relay's side
    // ----------------------------------------------------------------------- //

    /// Decide what to do with a datagram, from its header and where it came from.
    pub fn route(&self, header: &PacketHeader, from: SocketAddr) -> Route {
        let state = self.state();
        let Some(&user) = state.by_ssrc.get(&header.ssrc) else {
            return Route::Drop;
        };
        let Some(member) = state.members.get(&user) else {
            return Route::Drop;
        };

        if header.kind == MediaKind::Keepalive {
            let Some(key) = state.keys.get(&member.channel) else { return Route::Drop };
            return Route::Register { key: key.clone(), user };
        }

        // Everything else must come from the address the registration established.
        // Without this check, anyone who could guess a stream id could inject audio
        // into a call from anywhere on the internet.
        if member.addr != Some(from) {
            return Route::Drop;
        }

        // A sender may only use their own stream ids, and only for the right purpose:
        // voice on the voice ssrc, screen media on the screen ssrc. Otherwise a
        // client could send video on its voice stream and have it fan out to the whole
        // channel rather than to its subscribers.
        let expected_screen = header.kind.is_screen();
        let matches_stream = if expected_screen {
            header.ssrc == member.screen_ssrc
        } else {
            header.ssrc == member.voice_ssrc
        };
        if !matches_stream {
            return Route::Drop;
        }

        let targets = if expected_screen {
            state
                .members
                .iter()
                .filter(|(other, m)| {
                    **other != user && m.channel == member.channel && m.watching.contains(&user)
                })
                .filter_map(|(_, m)| m.addr)
                .collect()
        } else {
            state
                .members
                .iter()
                .filter(|(other, m)| {
                    // Deafened members are skipped at the relay rather than at the
                    // receiver. It is the same silence either way, and this way the
                    // packets are not sent at all — which on a self-hosted uplink is
                    // the difference that matters.
                    **other != user && m.channel == member.channel && !m.deafened
                })
                .filter_map(|(_, m)| m.addr)
                .collect()
        };
        Route::Forward(targets)
    }

    /// Record where a member's media comes from, after their registration packet has
    /// been verified. Returns the address to answer.
    pub fn bind_media_address(&self, user: Id, addr: SocketAddr) -> Option<SocketAddr> {
        let mut state = self.state();
        let member = state.members.get_mut(&user)?;
        if member.addr != Some(addr) {
            log::debug!("relay: {user} media address is now {addr}");
        }
        member.addr = Some(addr);
        member.last_seen = Instant::now();
        Some(addr)
    }

    /// How many members currently have a usable media address, for the status line.
    pub fn media_registered(&self) -> usize {
        self.state().members.values().filter(|m| m.addr.is_some()).count()
    }
}

impl State {
    fn take_ssrc(&mut self) -> u32 {
        // Wrapping past zero would hand out the "none" value and, worse, could
        // collide with a live stream. A server would have to run for a very long time
        // to get here; skipping zero costs one branch.
        loop {
            let ssrc = self.next_ssrc;
            self.next_ssrc = self.next_ssrc.wrapping_add(1);
            if ssrc != 0 && !self.by_ssrc.contains_key(&ssrc) {
                return ssrc;
            }
        }
    }

    /// Take a member out of the voice state entirely. Returns the channel they were
    /// in.
    fn remove_member(&mut self, user: Id) -> Option<Id> {
        let member = self.members.remove(&user)?;
        self.by_ssrc.remove(&member.voice_ssrc);
        self.by_ssrc.remove(&member.screen_ssrc);
        // Everybody who was watching this person stops watching, or their
        // subscription would survive into a later session with the same user id.
        for other in self.members.values_mut() {
            other.watching.remove(&user);
        }
        // The channel's key lives exactly as long as the channel is occupied. A new
        // key for the next call means somebody who left cannot decrypt it, which is
        // most of what per-session keying is for.
        if !self.members.values().any(|m| m.channel == member.channel) {
            self.keys.remove(&member.channel);
        }
        Some(member.channel)
    }
}

impl Member {
    fn state(&self, user: Id) -> VoiceState {
        VoiceState {
            user,
            channel: self.channel,
            muted: self.muted,
            deafened: self.deafened,
            ssrc: self.voice_ssrc,
            screen: self.screen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_proto::control::ScreenRequest;
    use tokio::sync::mpsc::unbounded_channel;

    fn hub() -> Hub {
        let dir = tempfile::tempdir().unwrap();
        let blobs = Blobs::open(dir.path().join("blobs")).unwrap();
        // The directory is dropped here; nothing in these tests touches the blob
        // store, and leaking a temp dir per test would be worse.
        Hub::new(
            Config::default(),
            Arc::new(Db::open_in_memory().unwrap()),
            Arc::new(blobs),
        )
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Join and register two people in one channel, returning their stream ids.
    fn two_in_a_call(hub: &Hub, channel: Id) -> (VoiceState, VoiceState) {
        let (ada, _, _) = hub.join_voice(Id(1), channel);
        let (bob, _, _) = hub.join_voice(Id(2), channel);
        hub.bind_media_address(Id(1), addr(1001));
        hub.bind_media_address(Id(2), addr(1002));
        (ada, bob)
    }

    #[test]
    fn presence_follows_the_last_connection_not_the_first() {
        let hub = hub();
        let (tx1, _rx1) = unbounded_channel();
        let (tx2, _rx2) = unbounded_channel();

        let (laptop, first) = hub.connect(Id(1), tx1);
        assert!(first, "the account was offline");
        let (phone, first) = hub.connect(Id(1), tx2);
        assert!(!first, "already online from the laptop");

        assert!(!hub.disconnect(laptop), "still signed in on the phone");
        assert!(hub.disconnect(phone), "now the account is offline");
        assert!(!hub.disconnect(phone), "and a second close changes nothing");
    }

    #[test]
    fn a_broadcast_reaches_everyone_and_can_skip_the_sender() {
        let hub = hub();
        let (tx1, mut rx1) = unbounded_channel();
        let (tx2, mut rx2) = unbounded_channel();
        let (ada, _) = hub.connect(Id(1), tx1);
        hub.connect(Id(2), tx2);

        hub.broadcast(ServerMsg::Pong { nonce: 1 });
        assert_eq!(rx1.try_recv().unwrap(), ServerMsg::Pong { nonce: 1 });
        assert_eq!(rx2.try_recv().unwrap(), ServerMsg::Pong { nonce: 1 });

        hub.broadcast_except(ada, ServerMsg::Pong { nonce: 2 });
        assert!(rx1.try_recv().is_err(), "the sender already knows");
        assert_eq!(rx2.try_recv().unwrap(), ServerMsg::Pong { nonce: 2 });
    }

    #[test]
    fn joining_a_second_channel_leaves_the_first() {
        let hub = hub();
        let (_, _, previous) = hub.join_voice(Id(1), Id(10));
        assert_eq!(previous, None);
        let (state, _, previous) = hub.join_voice(Id(1), Id(11));
        assert_eq!(previous, Some(Id(10)));
        assert_eq!(state.channel, Id(11));
        assert_eq!(hub.voice_states().len(), 1, "not in two channels at once");
    }

    #[test]
    fn every_stream_id_is_distinct_and_never_zero() {
        let hub = hub();
        let mut seen = HashSet::new();
        for user in 1..20u64 {
            let (state, _, _) = hub.join_voice(Id(user), Id(10));
            let screen = hub
                .start_screen(Id(user), ScreenRequest { width: 1, height: 1, fps: 1, kbps: 1, with_audio: false })
                .unwrap();
            for ssrc in [state.ssrc, screen.ssrc] {
                assert_ne!(ssrc, 0);
                assert!(seen.insert(ssrc), "{ssrc} was handed out twice");
            }
        }
    }

    #[test]
    fn a_channel_key_is_shared_while_occupied_and_replaced_once_empty() {
        let hub = hub();
        let (_, ada_key, _) = hub.join_voice(Id(1), Id(10));
        let (_, bob_key, _) = hub.join_voice(Id(2), Id(10));
        assert_eq!(ada_key, bob_key, "the channel has one key");

        // Somebody else's channel is a different key.
        let (_, other_key, _) = hub.join_voice(Id(3), Id(11));
        assert_ne!(ada_key, other_key);

        hub.leave_voice(Id(1));
        hub.leave_voice(Id(2));
        let (_, new_key, _) = hub.join_voice(Id(1), Id(10));
        assert_ne!(
            ada_key, new_key,
            "an emptied channel gets a new key, so leavers cannot decrypt the next call"
        );
    }

    #[test]
    fn voice_goes_to_the_channel_and_skips_the_deafened() {
        let hub = hub();
        let (ada, _) = two_in_a_call(&hub, Id(10));
        let header = PacketHeader { kind: MediaKind::Voice, ssrc: ada.ssrc, seq: 1, timestamp: 0 };

        let Route::Forward(targets) = hub.route(&header, addr(1001)) else {
            panic!("should forward")
        };
        assert_eq!(targets, vec![addr(1002)]);

        hub.set_voice_flags(Id(2), false, true);
        let Route::Forward(targets) = hub.route(&header, addr(1001)) else {
            panic!("should forward")
        };
        assert!(targets.is_empty(), "no point sending to somebody who is not listening");
    }

    #[test]
    fn a_member_with_no_registered_address_is_not_a_target() {
        let hub = hub();
        let (ada, _, _) = hub.join_voice(Id(1), Id(10));
        hub.join_voice(Id(2), Id(10));
        hub.bind_media_address(Id(1), addr(1001));
        // Bob has not sent a registration packet yet.
        let header = PacketHeader { kind: MediaKind::Voice, ssrc: ada.ssrc, seq: 1, timestamp: 0 };
        let Route::Forward(targets) = hub.route(&header, addr(1001)) else { panic!() };
        assert!(targets.is_empty());
    }

    /// The check that stops stream hijacking: after registration, packets are only
    /// accepted from the address that registered.
    #[test]
    fn packets_from_the_wrong_address_are_dropped() {
        let hub = hub();
        let (ada, _) = two_in_a_call(&hub, Id(10));
        let header = PacketHeader { kind: MediaKind::Voice, ssrc: ada.ssrc, seq: 1, timestamp: 0 };
        assert!(matches!(hub.route(&header, addr(9999)), Route::Drop));
    }

    #[test]
    fn an_unknown_stream_is_dropped_rather_than_answered() {
        let hub = hub();
        two_in_a_call(&hub, Id(10));
        let header = PacketHeader { kind: MediaKind::Voice, ssrc: 4242, seq: 1, timestamp: 0 };
        assert!(matches!(hub.route(&header, addr(1001)), Route::Drop));
    }

    /// Sending video on the voice stream would fan it out to the whole channel
    /// instead of to subscribers — a way to force a stream on people who did not
    /// ask for it, and to multiply the relay's uplink by the channel's size.
    #[test]
    fn a_stream_may_only_carry_what_it_is_for() {
        let hub = hub();
        let (ada, _) = two_in_a_call(&hub, Id(10));
        let screen = hub
            .start_screen(Id(1), ScreenRequest { width: 8, height: 8, fps: 30, kbps: 500, with_audio: false })
            .unwrap();

        let video_on_voice =
            PacketHeader { kind: MediaKind::VideoKey, ssrc: ada.ssrc, seq: 1, timestamp: 0 };
        assert!(matches!(hub.route(&video_on_voice, addr(1001)), Route::Drop));

        let voice_on_video =
            PacketHeader { kind: MediaKind::Voice, ssrc: screen.ssrc, seq: 1, timestamp: 0 };
        assert!(matches!(hub.route(&voice_on_video, addr(1001)), Route::Drop));
    }

    #[test]
    fn screen_media_goes_only_to_subscribers() {
        let hub = hub();
        two_in_a_call(&hub, Id(10));
        let share = hub
            .start_screen(Id(1), ScreenRequest { width: 8, height: 8, fps: 30, kbps: 500, with_audio: true })
            .unwrap();
        let video =
            PacketHeader { kind: MediaKind::VideoKey, ssrc: share.ssrc, seq: 1, timestamp: 0 };

        let Route::Forward(targets) = hub.route(&video, addr(1001)) else { panic!() };
        assert!(targets.is_empty(), "nobody is watching yet");

        assert!(hub.watch(Id(2), Id(1)));
        let Route::Forward(targets) = hub.route(&video, addr(1001)) else { panic!() };
        assert_eq!(targets, vec![addr(1002)]);

        hub.unwatch(Id(2), Id(1));
        let Route::Forward(targets) = hub.route(&video, addr(1001)) else { panic!() };
        assert!(targets.is_empty());
    }

    #[test]
    fn you_cannot_watch_somebody_in_another_channel() {
        let hub = hub();
        hub.join_voice(Id(1), Id(10));
        hub.join_voice(Id(2), Id(11));
        assert!(!hub.watch(Id(2), Id(1)));
        assert!(!hub.watch(Id(2), Id(3)), "or somebody who is not in a call at all");
    }

    /// A subscription that outlives the share it pointed at would make the relay
    /// think a viewer was already watching the *next* share, and silently deliver
    /// nothing.
    #[test]
    fn stopping_a_share_clears_every_subscription_to_it() {
        let hub = hub();
        two_in_a_call(&hub, Id(10));
        let share = hub
            .start_screen(Id(1), ScreenRequest { width: 8, height: 8, fps: 30, kbps: 500, with_audio: false })
            .unwrap();
        assert!(hub.watch(Id(2), Id(1)));
        assert_eq!(hub.stop_screen(Id(1)), Some(Id(10)));

        // A second share, and Bob has to opt in again.
        let share2 = hub
            .start_screen(Id(1), ScreenRequest { width: 8, height: 8, fps: 30, kbps: 500, with_audio: false })
            .unwrap();
        assert_eq!(share.ssrc, share2.ssrc, "the stream id is stable for the session");
        let video =
            PacketHeader { kind: MediaKind::VideoKey, ssrc: share2.ssrc, seq: 1, timestamp: 0 };
        let Route::Forward(targets) = hub.route(&video, addr(1001)) else { panic!() };
        assert!(targets.is_empty());
    }

    #[test]
    fn leaving_a_call_retires_its_stream_ids_and_subscriptions() {
        let hub = hub();
        let (ada, _) = two_in_a_call(&hub, Id(10));
        hub.start_screen(Id(1), ScreenRequest { width: 8, height: 8, fps: 30, kbps: 500, with_audio: false });
        assert!(hub.watch(Id(2), Id(1)));

        assert_eq!(hub.leave_voice(Id(1)), Some(Id(10)));
        let header = PacketHeader { kind: MediaKind::Voice, ssrc: ada.ssrc, seq: 1, timestamp: 0 };
        assert!(matches!(hub.route(&header, addr(1001)), Route::Drop), "the ssrc is retired");
        assert_eq!(hub.voice_peers(Id(2)), Vec::<Id>::new());
        assert_eq!(hub.leave_voice(Id(1)), None, "leaving twice is not an error");
    }

    #[test]
    fn a_keepalive_asks_for_verification_rather_than_being_trusted() {
        let hub = hub();
        let (state, key, _) = hub.join_voice(Id(1), Id(10));
        let key = SessionKey::from_base64(&key).unwrap();
        let header =
            PacketHeader { kind: MediaKind::Keepalive, ssrc: state.ssrc, seq: 0, timestamp: 0 };

        // Note the address: a keepalive is accepted from anywhere, because it is what
        // *establishes* the address. Its authenticity comes from the sealed payload.
        let Route::Register { key: routed, user } = hub.route(&header, addr(5555)) else {
            panic!("a keepalive should route to Register")
        };
        assert_eq!(user, Id(1));

        let mut datagram = Vec::new();
        key.seal(header, boa_proto::media::REGISTRATION_PLAINTEXT, &mut datagram).unwrap();
        assert!(routed.verify_registration(&datagram));

        assert_eq!(hub.bind_media_address(Id(1), addr(5555)), Some(addr(5555)));
        assert_eq!(hub.media_registered(), 1);
    }

    #[test]
    fn voice_states_are_reported_in_a_stable_order() {
        let hub = hub();
        for user in [Id(5), Id(2), Id(9)] {
            hub.join_voice(user, Id(10));
        }
        let states = hub.voice_states();
        assert_eq!(states.iter().map(|s| s.user).collect::<Vec<_>>(), vec![Id(2), Id(5), Id(9)]);
    }

    #[test]
    fn presence_decoration_marks_exactly_the_connected() {
        let hub = hub();
        let (tx, _rx) = unbounded_channel();
        hub.connect(Id(2), tx);
        let mut users = vec![
            User { id: Id(1), name: "ada".into(), display_name: String::new(), online: true },
            User { id: Id(2), name: "bob".into(), display_name: String::new(), online: false },
        ];
        hub.decorate_presence(&mut users);
        assert!(!users[0].online, "a stale flag is corrected, not trusted");
        assert!(users[1].online);
    }
}
