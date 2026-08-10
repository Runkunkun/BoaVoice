//! The UDP socket voice and video travel over.
//!
//! Plain blocking `std::net::UdpSocket` on dedicated threads, deliberately not tokio. The media path
//! has one sender and one receiver, both of which want to do exactly one thing in a tight loop with a
//! deadline; an async runtime buys multiplexing that is not needed and adds a scheduler between the
//! network and the encoder. A read timeout is all the "async" this needs, and it is what lets the
//! receive loop notice that it has been asked to stop.
//!
//! One socket carries every stream a client has — voice and screen — because that is what makes the
//! relay's address binding work: it learns the address from a registration packet, and a second
//! socket would be a second address it had never been told about.

use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use anyhow::{Context as _, Result};
use boa_proto::media::{MediaKind, PacketHeader, MAX_DATAGRAM, REGISTRATION_PLAINTEXT};
use boa_proto::SessionKey;

/// How long a receive waits before returning so the loop can check whether to stop.
///
/// 200 ms: short enough that leaving a call is immediate, long enough that an idle call is not
/// waking a thread five times a second for nothing.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// How often a keepalive goes out.
///
/// One second. It is doing three jobs, and the shortest of the three sets the interval: keeping a NAT
/// mapping alive (usually 30 s, sometimes much less), telling the relay where to send (only needed
/// once, but it has to be re-established after any network change), and being the client's own
/// evidence that the media path works at all — which is worth knowing within a second rather than
/// within thirty.
pub const KEEPALIVE: Duration = Duration::from_secs(1);

pub struct Transport {
    socket: UdpSocket,
    relay: SocketAddr,
    key: SessionKey,
}

impl Transport {
    /// Bind a socket and point it at the relay.
    pub fn open(relay: SocketAddr, key: SessionKey) -> Result<Transport> {
        // The local socket has to be the same family as the relay, or `send_to` fails with a
        // confusing "invalid argument" rather than anything about addresses.
        let bind: SocketAddr = if relay.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
        let socket = UdpSocket::bind(bind).with_context(|| format!("binding a UDP socket ({bind})"))?;
        socket.set_read_timeout(Some(READ_TIMEOUT)).context("setting a read timeout")?;

        // Not `connect`: a connected UDP socket refuses datagrams from any other address, and the
        // relay may legitimately answer from a different local address on a multi-homed host.
        log::info!("media: {} → relay {relay}", socket.local_addr()?);
        Ok(Transport { socket, relay, key })
    }

    /// A second handle on the same socket, for the other direction's thread.
    pub fn try_clone(&self) -> Result<Transport> {
        Ok(Transport {
            socket: self.socket.try_clone().context("cloning the media socket")?,
            relay: self.relay,
            key: self.key.clone(),
        })
    }

    pub fn key(&self) -> &SessionKey {
        &self.key
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    /// Seal and send one packet, reusing `scratch` so a frame costs no allocation.
    pub fn send(&self, header: PacketHeader, payload: &[u8], scratch: &mut Vec<u8>) -> Result<()> {
        self.key.seal(header, payload, scratch).map_err(|err| anyhow::anyhow!("sealing: {err}"))?;
        self.socket.send_to(scratch, self.relay).context("sending a media packet")?;
        Ok(())
    }

    /// Tell the relay where to send our media, and prove we belong to the session.
    pub fn register(&self, ssrc: u32, seq: u32, scratch: &mut Vec<u8>) -> Result<()> {
        let header = PacketHeader { kind: MediaKind::Keepalive, ssrc, seq, timestamp: 0 };
        self.send(header, REGISTRATION_PLAINTEXT, scratch)
    }

    /// Wait for a packet. `Ok(None)` means the read timed out, which is not an error — it is how the
    /// loop gets a chance to check whether it should stop.
    pub fn recv(&self, buffer: &mut [u8; MAX_DATAGRAM]) -> Result<Option<(PacketHeader, Vec<u8>)>> {
        match self.socket.recv_from(buffer) {
            Ok((len, from)) => {
                // Anything not from the relay is dropped without a word: on the open internet a port
                // that answers strangers is a port strangers keep talking to.
                if from.ip() != self.relay.ip() {
                    log::debug!("media: ignoring a datagram from {from}");
                    return Ok(None);
                }
                match self.key.open(&buffer[..len]) {
                    Ok(opened) => Ok(Some(opened)),
                    // A packet that will not authenticate is not worth an error either: it is either
                    // corruption or somebody guessing, and both should cost one log line at most.
                    Err(err) => {
                        log::debug!("media: undecipherable packet from {from}: {err}");
                        Ok(None)
                    }
                }
            }
            Err(err) if is_timeout(&err) => Ok(None),
            // An ICMP "port unreachable" from an earlier send arrives here on some platforms. The
            // relay coming back up must not require restarting the call.
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                log::debug!("media: {err}");
                Ok(None)
            }
            Err(err) => Err(err).context("receiving a media packet"),
        }
    }
}

fn is_timeout(err: &std::io::Error) -> bool {
    // Two kinds, because the platforms disagree: BSDs and macOS report `WouldBlock`, Linux
    // `TimedOut`. Treating only one as a timeout makes the receive loop exit on the other.
    matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in relay: a socket that receives, and can send back.
    fn fake_relay() -> (UdpSocket, SocketAddr) {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        (socket, addr)
    }

    #[test]
    fn a_registration_packet_is_what_the_relay_expects() {
        let (relay, relay_addr) = fake_relay();
        let key = SessionKey::random();
        let transport = Transport::open(relay_addr, key.clone()).unwrap();

        let mut scratch = Vec::new();
        transport.register(7, 0, &mut scratch).unwrap();

        let mut buffer = [0u8; MAX_DATAGRAM];
        let (len, _) = relay.recv_from(&mut buffer).unwrap();
        // The exact check the relay performs.
        assert!(key.verify_registration(&buffer[..len]));
    }

    #[test]
    fn a_packet_round_trips_through_a_relay_that_echoes() {
        let (relay, relay_addr) = fake_relay();
        let key = SessionKey::random();
        let transport = Transport::open(relay_addr, key).unwrap();

        let header = PacketHeader { kind: MediaKind::Voice, ssrc: 3, seq: 9, timestamp: 480 };
        let mut scratch = Vec::new();
        transport.send(header, b"an opus frame", &mut scratch).unwrap();

        let mut buffer = [0u8; MAX_DATAGRAM];
        let (len, from) = relay.recv_from(&mut buffer).unwrap();
        relay.send_to(&buffer[..len], from).unwrap();

        let mut inbound = [0u8; MAX_DATAGRAM];
        let (got, payload) = transport.recv(&mut inbound).unwrap().expect("the echo should arrive");
        assert_eq!(got, header);
        assert_eq!(payload, b"an opus frame");
    }

    #[test]
    fn a_read_that_times_out_is_not_an_error() {
        let (_relay, relay_addr) = fake_relay();
        let transport = Transport::open(relay_addr, SessionKey::random()).unwrap();
        let mut buffer = [0u8; MAX_DATAGRAM];
        // Nothing is going to arrive; the point is that this returns rather than blocking forever or
        // reporting a failure, which is what lets the receive loop notice a stop request.
        assert!(transport.recv(&mut buffer).unwrap().is_none());
    }

    #[test]
    fn a_packet_under_the_wrong_key_is_dropped_rather_than_raised() {
        let (relay, relay_addr) = fake_relay();
        let transport = Transport::open(relay_addr, SessionKey::random()).unwrap();

        // Somebody else's session, sent from the relay's address.
        let stranger = SessionKey::random();
        let mut datagram = Vec::new();
        stranger
            .seal(
                PacketHeader { kind: MediaKind::Voice, ssrc: 1, seq: 0, timestamp: 0 },
                b"not for us",
                &mut datagram,
            )
            .unwrap();
        // Through the loopback address explicitly: the socket is bound to `0.0.0.0`, and sending to
        // an unspecified address is not routable.
        let port = transport.local_addr().unwrap().port();
        relay.send_to(&datagram, SocketAddr::from(([127, 0, 0, 1], port))).unwrap();

        let mut buffer = [0u8; MAX_DATAGRAM];
        assert!(transport.recv(&mut buffer).unwrap().is_none());
    }

    #[test]
    fn both_platforms_timeout_kinds_are_recognised() {
        use std::io::{Error, ErrorKind};
        // macOS and the BSDs report one, Linux the other. Treating only one as a timeout makes the
        // receive loop exit on the wrong platform.
        assert!(is_timeout(&Error::from(ErrorKind::WouldBlock)));
        assert!(is_timeout(&Error::from(ErrorKind::TimedOut)));
        assert!(!is_timeout(&Error::from(ErrorKind::AddrInUse)));
    }

    #[test]
    fn the_socket_family_follows_the_relays() {
        let key = SessionKey::random();
        let v4 = Transport::open("127.0.0.1:1".parse().unwrap(), key.clone()).unwrap();
        assert!(v4.local_addr().unwrap().is_ipv4());
        // IPv6 may be unavailable in a container, so this is a check rather than an assertion about
        // the machine.
        if let Ok(v6) = Transport::open("[::1]:1".parse().unwrap(), key) {
            assert!(v6.local_addr().unwrap().is_ipv6());
        }
    }
}
