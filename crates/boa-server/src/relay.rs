//! The media relay: one UDP socket, forwarding sealed packets between the people
//! who are allowed to have them.
//!
//! Deliberately the dumbest part of the server. It does not decode audio, mix it,
//! transcode video or decide what quality anybody should get — a full SFU does all
//! of that, and every bit of it is CPU on the box the operator is paying for, and
//! quality decisions made in the middle are exactly the thing this project exists
//! not to have. Here, a packet arrives, [`Hub::route`] says who should get it, and
//! the same bytes go out. Two people talking cost two datagrams in and two out, and
//! the encoder's settings are whatever the sender chose.
//!
//! What it *does* enforce is who may send what to whom: a stream is bound to the
//! address that registered it, a stream may only carry the kind of media it was
//! allocated for, and screen media only reaches subscribers. All three checks live in
//! [`Hub::route`]; this module is the loop around them.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use boa_proto::media::{HEADER_LEN, MAX_DATAGRAM};
use boa_proto::PacketHeader;
use tokio::net::UdpSocket;

use crate::hub::{Hub, Route};

/// Counters for the status line. Relaxed ordering throughout: these are for a human
/// reading a log, and a count that is a few packets stale is not wrong in any way
/// that matters.
#[derive(Default)]
pub struct Stats {
    pub received: AtomicU64,
    pub forwarded: AtomicU64,
    pub dropped: AtomicU64,
}

impl Stats {
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.received.load(Ordering::Relaxed),
            self.forwarded.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

/// Bind the media socket and forward until the process ends.
pub async fn run(hub: Arc<Hub>, bind: SocketAddr, stats: Arc<Stats>) -> Result<()> {
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("binding UDP {bind}"))?;
    log::info!("relay: listening on UDP {}", socket.local_addr()?);
    serve(hub, socket, stats).await
}

/// The loop itself, over an already-bound socket, so a test can drive it.
pub async fn serve(hub: Arc<Hub>, socket: UdpSocket, stats: Arc<Stats>) -> Result<()> {
    // One packet longer than the protocol allows, so an oversized datagram is
    // *recognised* as oversized rather than silently truncated into something that
    // still parses. A truncated packet would fail its authentication tag anyway, but
    // it would fail it after being forwarded to everyone.
    let mut buffer = vec![0u8; MAX_DATAGRAM + 1];

    loop {
        let (len, from) = match socket.recv_from(&mut buffer).await {
            Ok(result) => result,
            // A `send_to` to an unreachable peer can be reported here on some
            // platforms, against the *next* receive. That is not a reason to stop
            // relaying for everybody else.
            Err(err) if is_transient(&err) => {
                log::debug!("relay: transient receive error: {err}");
                continue;
            }
            Err(err) => return Err(err).context("receiving on the media socket"),
        };
        stats.received.fetch_add(1, Ordering::Relaxed);

        let datagram = &buffer[..len];
        if !(HEADER_LEN..=MAX_DATAGRAM).contains(&len) {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Ok(header) = PacketHeader::decode(datagram) else {
            // Not ours at all: a port scan, or a stray packet from another protocol.
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        match hub.route(&header, from) {
            Route::Register { key, user } => {
                // The payload has to open under the channel's key *and* contain the
                // registration plaintext. Anything else — including a replayed voice
                // packet relabelled as a keepalive — is dropped, because this is the
                // one message that can move where somebody's audio is delivered.
                if !key.verify_registration(datagram) {
                    log::debug!("relay: bogus registration for ssrc {} from {from}", header.ssrc);
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                hub.bind_media_address(user, from);
                // Echoed back, which does two jobs: it tells the client its packets
                // are arriving (the only way it can know, over UDP), and it keeps the
                // NAT mapping warm from this side as well as from the client's.
                if let Err(err) = socket.send_to(datagram, from).await {
                    log::debug!("relay: keepalive reply to {from}: {err}");
                }
            }
            Route::Forward(targets) => {
                for target in targets {
                    match socket.send_to(datagram, target).await {
                        Ok(_) => {
                            stats.forwarded.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(err) => {
                            // One unreachable listener must not cost the others their
                            // copy, so this is logged and stepped over. It happens
                            // routinely: a client that quit abruptly leaves an address
                            // that answers with ICMP unreachable until the control
                            // plane notices.
                            log::debug!("relay: forwarding to {target}: {err}");
                            stats.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            Route::Drop => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Whether a socket error is one to carry on from.
///
/// `ConnectionReset` and `ConnectionRefused` on a *UDP* socket are the platform
/// reporting an ICMP error from an earlier `send_to`, delivered against whatever call
/// comes next. Treating them as fatal would let any client that quit at the wrong
/// moment take the whole relay down with it.
fn is_transient(err: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(err.kind(), ConnectionReset | ConnectionRefused | ConnectionAborted | Interrupted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blobs::Blobs;
    use crate::config::Config;
    use crate::db::Db;
    use boa_proto::media::REGISTRATION_PLAINTEXT;
    use boa_proto::{Id, MediaKind, SessionKey};
    use std::time::Duration;

    fn hub() -> Arc<Hub> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(Hub::new(
            Config::default(),
            Arc::new(Db::open_in_memory().unwrap()),
            Arc::new(Blobs::open(dir.path().join("blobs")).unwrap()),
        ))
    }

    /// A client socket, plus the little bit of ceremony every packet needs.
    struct Peer {
        socket: UdpSocket,
        key: SessionKey,
        ssrc: u32,
        seq: u32,
    }

    impl Peer {
        async fn new(key: &str, ssrc: u32) -> Self {
            Peer {
                socket: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
                key: SessionKey::from_base64(key).unwrap(),
                ssrc,
                seq: 0,
            }
        }

        async fn send(&mut self, relay: SocketAddr, kind: MediaKind, payload: &[u8]) {
            let header = PacketHeader { kind, ssrc: self.ssrc, seq: self.seq, timestamp: 0 };
            self.seq += 1;
            let mut datagram = Vec::new();
            self.key.seal(header, payload, &mut datagram).unwrap();
            self.socket.send_to(&datagram, relay).await.unwrap();
        }

        async fn register(&mut self, relay: SocketAddr) {
            self.send(relay, MediaKind::Keepalive, REGISTRATION_PLAINTEXT).await;
            // The relay echoes it, which is how a real client knows it got through.
            let mut buffer = [0u8; MAX_DATAGRAM];
            let len = tokio::time::timeout(Duration::from_secs(2), self.socket.recv(&mut buffer))
                .await
                .expect("the relay should answer a good registration")
                .unwrap();
            assert!(self.key.open(&buffer[..len]).is_ok());
        }

        /// The next packet, or `None` if nothing arrives promptly.
        async fn recv(&self) -> Option<(PacketHeader, Vec<u8>)> {
            let mut buffer = [0u8; MAX_DATAGRAM];
            let len = tokio::time::timeout(Duration::from_millis(300), self.socket.recv(&mut buffer))
                .await
                .ok()?
                .ok()?;
            self.key.open(&buffer[..len]).ok()
        }
    }

    /// The relay end to end, over real sockets: two people in a call, one talks, the
    /// other hears it — and a third party who never registered hears nothing.
    #[tokio::test]
    async fn a_voice_packet_reaches_the_other_person_in_the_channel() {
        let hub = hub();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(hub.clone(), socket, Arc::new(Stats::default())));

        let (ada_state, key, _) = hub.join_voice(Id(1), Id(10));
        let (bob_state, _, _) = hub.join_voice(Id(2), Id(10));

        let mut ada = Peer::new(&key, ada_state.ssrc).await;
        let mut bob = Peer::new(&key, bob_state.ssrc).await;
        ada.register(relay_addr).await;
        bob.register(relay_addr).await;

        ada.send(relay_addr, MediaKind::Voice, b"an opus frame").await;
        let (header, payload) = bob.recv().await.expect("bob should hear ada");
        assert_eq!(header.ssrc, ada_state.ssrc, "and knows who it was");
        assert_eq!(payload, b"an opus frame");

        // Nothing comes back to the sender.
        assert!(ada.recv().await.is_none());
    }

    #[tokio::test]
    async fn an_unregistered_stream_is_ignored() {
        let hub = hub();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = socket.local_addr().unwrap();
        let stats = Arc::new(Stats::default());
        tokio::spawn(serve(hub.clone(), socket, stats.clone()));

        let (ada_state, key, _) = hub.join_voice(Id(1), Id(10));
        let (bob_state, _, _) = hub.join_voice(Id(2), Id(10));
        let mut ada = Peer::new(&key, ada_state.ssrc).await;
        let bob = Peer::new(&key, bob_state.ssrc).await;
        ada.register(relay_addr).await;
        // Bob never registers, so the relay does not know where to send to him.

        ada.send(relay_addr, MediaKind::Voice, b"hello?").await;
        assert!(bob.recv().await.is_none());
    }

    /// A registration that does not open under the channel key must not move the
    /// address binding — otherwise anyone who saw a header go past could redirect
    /// somebody else's audio to themselves.
    #[tokio::test]
    async fn a_forged_registration_does_not_take_over_a_stream() {
        let hub = hub();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(hub.clone(), socket, Arc::new(Stats::default())));

        let (ada_state, key, _) = hub.join_voice(Id(1), Id(10));
        let (bob_state, _, _) = hub.join_voice(Id(2), Id(10));
        let mut ada = Peer::new(&key, ada_state.ssrc).await;
        let mut bob = Peer::new(&key, bob_state.ssrc).await;
        ada.register(relay_addr).await;
        bob.register(relay_addr).await;

        // An attacker with the right ssrc and the wrong key.
        let mut attacker = Peer::new(&SessionKey::random().to_base64(), ada_state.ssrc).await;
        attacker.send(relay_addr, MediaKind::Keepalive, REGISTRATION_PLAINTEXT).await;
        assert!(
            attacker.recv().await.is_none(),
            "a bad registration gets no reply, so it cannot even be probed"
        );

        // Ada's audio still goes to Bob and not to the attacker.
        ada.send(relay_addr, MediaKind::Voice, b"still me").await;
        assert!(bob.recv().await.is_some());
        assert!(attacker.recv().await.is_none());
    }

    #[tokio::test]
    async fn rubbish_on_the_port_is_counted_and_forgotten() {
        let hub = hub();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = socket.local_addr().unwrap();
        let stats = Arc::new(Stats::default());
        tokio::spawn(serve(hub.clone(), socket, stats.clone()));

        let noise = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        noise.send_to(b"GET / HTTP/1.1", relay_addr).await.unwrap();
        noise.send_to(&[0u8; 4], relay_addr).await.unwrap();
        noise.send_to(&vec![0u8; MAX_DATAGRAM + 1], relay_addr).await.unwrap();

        // Give the loop a moment, then check it is still alive by doing real work.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (received, _, dropped) = stats.snapshot();
        assert!(received >= 3, "{received}");
        assert!(dropped >= 3, "{dropped}");

        let (ada_state, key, _) = hub.join_voice(Id(1), Id(10));
        let mut ada = Peer::new(&key, ada_state.ssrc).await;
        ada.register(relay_addr).await;
    }

    #[tokio::test]
    async fn screen_video_reaches_a_watcher_and_nobody_else() {
        let hub = hub();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = socket.local_addr().unwrap();
        tokio::spawn(serve(hub.clone(), socket, Arc::new(Stats::default())));

        let (ada_state, key, _) = hub.join_voice(Id(1), Id(10));
        let (bob_state, _, _) = hub.join_voice(Id(2), Id(10));
        let (carol_state, _, _) = hub.join_voice(Id(3), Id(10));
        let share = hub
            .start_screen(
                Id(1),
                boa_proto::control::ScreenRequest {
                    width: 1920,
                    height: 1080,
                    fps: 60,
                    kbps: 20_000,
                    with_audio: false,
                },
            )
            .unwrap();

        let mut ada_voice = Peer::new(&key, ada_state.ssrc).await;
        ada_voice.register(relay_addr).await;
        // The watchers need a media address before anything can be delivered to them.
        // In a real client one socket carries both of that person's streams, which is
        // why registering the voice stream is enough to receive video.
        let mut bob = Peer::new(&key, bob_state.ssrc).await;
        let mut carol = Peer::new(&key, carol_state.ssrc).await;
        bob.register(relay_addr).await;
        carol.register(relay_addr).await;

        assert!(hub.watch(Id(2), Id(1)), "bob subscribes");

        // Ada's video goes out on the screen stream, from the same socket.
        let header =
            PacketHeader { kind: MediaKind::VideoKey, ssrc: share.ssrc, seq: 0, timestamp: 0 };
        let mut datagram = Vec::new();
        SessionKey::from_base64(&key)
            .unwrap()
            .seal(header, b"a keyframe", &mut datagram)
            .unwrap();
        ada_voice.socket.send_to(&datagram, relay_addr).await.unwrap();

        let (got, payload) = bob.recv().await.expect("the watcher gets the frame");
        assert_eq!(got.kind, MediaKind::VideoKey);
        assert_eq!(payload, b"a keyframe");
        assert!(carol.recv().await.is_none(), "carol did not ask for it");
    }

    #[test]
    fn icmp_errors_reported_on_receive_are_not_fatal() {
        use std::io::{Error, ErrorKind};
        assert!(is_transient(&Error::from(ErrorKind::ConnectionReset)));
        assert!(is_transient(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(!is_transient(&Error::from(ErrorKind::AddrInUse)));
    }
}
