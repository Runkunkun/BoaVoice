//! The media plane: voice and video packets over UDP.
//!
//! A packet is a 16-byte plaintext header followed by an AEAD-sealed payload:
//!
//! ```text
//!  0      2    3    4        8        12       16
//! +------+----+----+--------+--------+--------+------------------+
//! |magic |ver |kind| ssrc   | seq    | tstamp | sealed payload   |
//! +------+----+----+--------+--------+--------+------------------+
//!    u16   u8   u8    u32      u32      u32      ciphertext+tag
//! ```
//!
//! **Why the header is in the clear.** The relay has to route the packet, and
//! routing needs the stream id. Encrypting it would mean the relay holds the keys
//! *in the hot path* — a decrypt-and-re-encrypt per recipient per 20 ms frame per
//! speaker — instead of doing a map lookup and a `send_to`. The header is also the
//! AEAD's associated data, so it is authenticated even though it is readable: a
//! relay or an attacker can *see* who a packet claims to be from, and cannot
//! change it without the payload failing to open.
//!
//! **Why UDP.** Voice is only useful if it is late by very little. A 20 ms frame
//! retransmitted 80 ms later has missed the moment it belonged to, so the right
//! response to loss is to carry on, which is exactly what TCP will not do. The one
//! thing TCP would give — ordering — is cheaper to rebuild from [`PacketHeader::seq`]
//! in the jitter buffer than to pay for on every packet.
//!
//! **Nonce construction.** All members of a voice session share one key, so the
//! nonce must be unique across senders as well as across packets. It is
//! `ssrc ‖ seq ‖ kind ‖ 0 0 0`, and each of those three parts is load-bearing:
//! `ssrc` separates senders, `seq` separates packets, and `kind` separates a
//! sender's audio stream from their video stream, which have independent sequence
//! counters and would otherwise collide on their first packets. Reusing a
//! (key, nonce) pair with this cipher does not merely leak a plaintext — it leaks
//! the authentication key, so this is the one thing in the module that a test
//! guards directly.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;

/// `0xB0A1` — "boa 1". Lets the relay drop a stray datagram (a port scan, a
/// stale packet from another protocol) before it costs a map lookup.
pub const MAGIC: u16 = 0xB0A1;

/// Media-plane revision, separate from [`crate::PROTOCOL_VERSION`] because the
/// packet layout can outlive several control-plane changes.
pub const MEDIA_VERSION: u8 = 1;

/// Bytes before the payload starts.
pub const HEADER_LEN: usize = 16;

/// The AEAD tag, appended to every sealed payload.
pub const TAG_LEN: usize = 16;

/// Largest datagram the protocol will emit.
///
/// 1200 bytes, not 1500. The number that matters is the smallest MTU on the path,
/// and 1500 is only the *local* Ethernet one: a packet that size arrives fragmented
/// or not at all over PPPoE (1492), most VPNs (1400 or less) and any tunnel with an
/// encapsulation header. IP fragments are worse than smaller packets, because
/// losing one fragment loses the whole datagram. 1200 is the figure WebRTC settled
/// on for the same reason and it clears every tunnel in ordinary use.
pub const MAX_DATAGRAM: usize = 1200;

/// Largest plaintext that fits in one datagram.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN - TAG_LEN;

/// Voice sample rate. Opus's native rate; anything else means resampling twice for
/// no gain, since the codec would convert internally anyway.
pub const VOICE_SAMPLE_RATE: u32 = 48_000;

/// Voice is mono.
///
/// Stereo would double the uplink to place people in a field nobody is looking at,
/// and it doubles the mixer's work on the receiving side, where the real cost is:
/// a listener mixes *every* speaker. Desktop audio from a screen share is a
/// separate stream and is stereo, because there the channel separation is content.
pub const VOICE_CHANNELS: u16 = 1;

/// Milliseconds of audio per packet.
///
/// 20 ms is the balance point. Shorter frames cut latency but multiply per-packet
/// overhead — at 10 ms the 16-byte header plus 28 bytes of UDP/IP is a third of
/// the traffic — and multiply the syscall rate on a relay that forwards for
/// everyone. Longer frames make every lost packet a longer hole.
pub const VOICE_FRAME_MS: u32 = 20;

/// Samples per channel in one voice frame: 960.
pub const VOICE_FRAME_SAMPLES: usize = (VOICE_SAMPLE_RATE * VOICE_FRAME_MS / 1000) as usize;

/// What a packet carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MediaKind {
    /// One Opus frame of voice, mono, [`VOICE_FRAME_MS`] long.
    Voice = 0,
    /// A video fragment belonging to a keyframe. Marked separately from
    /// [`MediaKind::VideoDelta`] so a decoder joining mid-stream — or recovering
    /// from loss — can discard deltas until a keyframe arrives instead of feeding
    /// H.264 fragments it has no reference for, which produces the smear of green
    /// blocks everyone recognises.
    VideoKey = 1,
    VideoDelta = 2,
    /// Address registration and liveness. Carries a sealed, fixed plaintext, which
    /// is what proves the sender holds the session key — see
    /// [`REGISTRATION_PLAINTEXT`].
    Keepalive = 3,
    /// Stereo Opus from a shared screen's audio. A separate kind rather than a
    /// second voice stream, so a viewer who only wants the picture can be sent the
    /// picture.
    DesktopAudio = 4,
}

impl MediaKind {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => MediaKind::Voice,
            1 => MediaKind::VideoKey,
            2 => MediaKind::VideoDelta,
            3 => MediaKind::Keepalive,
            4 => MediaKind::DesktopAudio,
            _ => return None,
        })
    }

    /// Whether this kind is part of a screen share, and so goes only to
    /// subscribers.
    pub fn is_screen(self) -> bool {
        matches!(self, MediaKind::VideoKey | MediaKind::VideoDelta | MediaKind::DesktopAudio)
    }

    pub fn is_video(self) -> bool {
        matches!(self, MediaKind::VideoKey | MediaKind::VideoDelta)
    }
}

/// The plaintext inside a [`MediaKind::Keepalive`].
///
/// The relay learns a client's UDP address from the first keepalive it sees for an
/// ssrc, which is a hijack waiting to happen if the packet is unauthenticated:
/// anyone who watched a header go past could redirect somebody's audio to
/// themselves by sending one packet. Requiring the payload to open under the
/// session key means only a member of the session can move the binding.
pub const REGISTRATION_PLAINTEXT: &[u8] = b"boavoice/register/1";

#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum MediaError {
    #[error("datagram too short ({0} bytes)")]
    TooShort(usize),
    #[error("not a BoaVoice packet")]
    BadMagic,
    #[error("media version {0} is not supported")]
    BadVersion(u8),
    #[error("unknown packet kind {0}")]
    BadKind(u8),
    #[error("payload failed to authenticate")]
    NotAuthentic,
    #[error("payload of {0} bytes does not fit a datagram")]
    TooLarge(usize),
}

/// The plaintext part of a packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PacketHeader {
    pub kind: MediaKind,
    /// Which stream this is. Unique per (user, purpose) within a session; the
    /// control plane hands them out.
    pub ssrc: u32,
    /// Per-(ssrc, kind) counter, incremented once per packet and never reset
    /// within a session — it is half the AEAD nonce.
    pub seq: u32,
    /// For voice, the sample index of the frame's first sample (48 kHz), which is
    /// what lets a jitter buffer tell "the next frame" from "the frame after a
    /// 100 ms silence" without a separate marker. For video, milliseconds since
    /// the share started, shared by every fragment of one frame — which is also
    /// how the reassembler groups them.
    pub timestamp: u32,
}

impl PacketHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        out[2] = MEDIA_VERSION;
        out[3] = self.kind as u8;
        out[4..8].copy_from_slice(&self.ssrc.to_be_bytes());
        out[8..12].copy_from_slice(&self.seq.to_be_bytes());
        out[12..16].copy_from_slice(&self.timestamp.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, MediaError> {
        if bytes.len() < HEADER_LEN {
            return Err(MediaError::TooShort(bytes.len()));
        }
        let magic = u16::from_be_bytes([bytes[0], bytes[1]]);
        if magic != MAGIC {
            return Err(MediaError::BadMagic);
        }
        if bytes[2] != MEDIA_VERSION {
            return Err(MediaError::BadVersion(bytes[2]));
        }
        let kind = MediaKind::from_u8(bytes[3]).ok_or(MediaError::BadKind(bytes[3]))?;
        Ok(PacketHeader {
            kind,
            ssrc: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            seq: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            timestamp: u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        })
    }

    /// The AEAD nonce for this packet. See the module docs for why each part is
    /// there.
    fn nonce(&self) -> Nonce {
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&self.ssrc.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.seq.to_be_bytes());
        bytes[8] = self.kind as u8;
        *Nonce::from_slice(&bytes)
    }
}

/// A voice session's symmetric key.
///
/// One per voice session, handed to every member by the server. That makes this
/// transport encryption and not end-to-end: it stops everyone on the network path,
/// and not the server, which generated the key. The relay does not *use* it except
/// to check registration packets, but it could. The honest summary is in the
/// README; the fix is per-pair key agreement and is not what this is.
#[derive(Clone)]
pub struct SessionKey {
    cipher: ChaCha20Poly1305,
    bytes: [u8; 32],
}

impl SessionKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&bytes));
        SessionKey { cipher, bytes }
    }

    /// A fresh key from the OS entropy source.
    pub fn random() -> Self {
        let mut bytes = [0u8; 32];
        // `rand::rng()` is seeded from the OS and reseeds itself; going straight to
        // `getrandom` per key would be more obviously correct and is a syscall on a
        // path that runs once per voice session, so either is fine. This one keeps
        // the dependency list shorter.
        rand::rng().fill_bytes(&mut bytes);
        Self::from_bytes(bytes)
    }

    /// Base64, for the control plane's JSON.
    pub fn to_base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(self.bytes)
    }

    pub fn from_base64(text: &str) -> Option<Self> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
        let bytes: [u8; 32] = bytes.try_into().ok()?;
        Some(Self::from_bytes(bytes))
    }

    /// Build a complete datagram: header, then `plaintext` sealed against it.
    ///
    /// Writes into `out` rather than returning a `Vec`, because this runs fifty
    /// times a second per stream and the caller has a buffer already. `out` is
    /// cleared first, so a stale longer packet cannot leave a tail behind.
    pub fn seal(
        &self,
        header: PacketHeader,
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), MediaError> {
        if plaintext.len() + HEADER_LEN + TAG_LEN > MAX_DATAGRAM {
            return Err(MediaError::TooLarge(plaintext.len()));
        }
        let head = header.encode();
        out.clear();
        out.extend_from_slice(&head);
        out.extend_from_slice(plaintext);

        // In place, over the slice past the header: the alternative (`encrypt`,
        // which allocates and returns a Vec) would allocate twice per packet per
        // stream. The header is passed as associated data, so it is authenticated
        // without being hidden.
        let mut buffer = InPlace { data: out, from: HEADER_LEN };
        self.cipher
            .encrypt_in_place(&header.nonce(), &head, &mut buffer)
            .map_err(|_| MediaError::NotAuthentic)
    }

    /// Verify and decrypt a datagram, returning its header and plaintext.
    pub fn open(&self, datagram: &[u8]) -> Result<(PacketHeader, Vec<u8>), MediaError> {
        let header = PacketHeader::decode(datagram)?;
        if datagram.len() < HEADER_LEN + TAG_LEN {
            return Err(MediaError::TooShort(datagram.len()));
        }
        let head = &datagram[..HEADER_LEN];
        let mut payload = datagram[HEADER_LEN..].to_vec();
        self.cipher
            .decrypt_in_place(&header.nonce(), head, &mut payload)
            .map_err(|_| MediaError::NotAuthentic)?;
        Ok((header, payload))
    }

    /// Whether `datagram` is a valid registration packet for its own ssrc.
    ///
    /// The relay's only use of the key. Deliberately not "does it decrypt" but
    /// "does it decrypt *to the expected plaintext*" — a replayed voice packet
    /// relabelled as a keepalive would pass the first test and fail this one.
    pub fn verify_registration(&self, datagram: &[u8]) -> bool {
        match self.open(datagram) {
            Ok((header, payload)) => {
                header.kind == MediaKind::Keepalive && payload == REGISTRATION_PLAINTEXT
            }
            Err(_) => false,
        }
    }
}

impl std::fmt::Debug for SessionKey {
    /// Never print the key. A voice session key in a log file is a recording of the
    /// call for anyone who also captured the packets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(redacted)")
    }
}

/// `aead::Buffer` over the payload region of a datagram.
///
/// `Vec<u8>` already implements the trait, but only for the whole vector — and the
/// header must stay outside the ciphertext. This view lets the cipher extend and
/// truncate the tail while the header sits in front of it untouched.
struct InPlace<'a> {
    data: &'a mut Vec<u8>,
    from: usize,
}

impl AsRef<[u8]> for InPlace<'_> {
    fn as_ref(&self) -> &[u8] {
        &self.data[self.from..]
    }
}

impl AsMut<[u8]> for InPlace<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.from..]
    }
}

impl chacha20poly1305::aead::Buffer for InPlace<'_> {
    fn extend_from_slice(&mut self, other: &[u8]) -> chacha20poly1305::aead::Result<()> {
        self.data.extend_from_slice(other);
        Ok(())
    }

    fn truncate(&mut self, len: usize) {
        self.data.truncate(self.from + len);
    }

    fn len(&self) -> usize {
        self.data.len() - self.from
    }

    fn is_empty(&self) -> bool {
        self.data.len() == self.from
    }
}

// --------------------------------------------------------------------------- //
// Video fragmentation
// --------------------------------------------------------------------------- //

/// Bytes of fragment header that precede video data *inside* the sealed payload.
///
/// Inside rather than in the plaintext header on purpose: the relay forwards video
/// whole and has no reason to know how a frame was cut up, and every field in the
/// clear is a field an observer can use to fingerprint what is on screen.
pub const FRAG_HEADER_LEN: usize = 4;

/// Video bytes that fit in one datagram.
pub const MAX_VIDEO_CHUNK: usize = MAX_PAYLOAD - FRAG_HEADER_LEN;

/// Split one encoded frame into datagram-sized pieces.
///
/// Returns `(index, count, chunk)` per piece. An empty frame yields nothing rather
/// than one empty fragment — an encoder that produced no output for a frame (which
/// happens, when nothing changed) should cost no packets.
pub fn fragment(frame: &[u8]) -> impl Iterator<Item = (u16, u16, &[u8])> {
    let count = frame.len().div_ceil(MAX_VIDEO_CHUNK);
    frame
        .chunks(MAX_VIDEO_CHUNK)
        .enumerate()
        .map(move |(index, chunk)| (index as u16, count as u16, chunk))
}

/// Write a fragment payload — header then data — into `out`.
pub fn write_fragment(index: u16, count: u16, chunk: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(chunk);
}

/// Read a fragment payload back.
pub fn read_fragment(payload: &[u8]) -> Option<(u16, u16, &[u8])> {
    if payload.len() < FRAG_HEADER_LEN {
        return None;
    }
    let index = u16::from_be_bytes([payload[0], payload[1]]);
    let count = u16::from_be_bytes([payload[2], payload[3]]);
    // A fragment claiming to be number 5 of 3 is corrupt (or hostile); the caller
    // would otherwise index a vector out of bounds.
    if count == 0 || index >= count {
        return None;
    }
    Some((index, count, &payload[FRAG_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: MediaKind, seq: u32) -> PacketHeader {
        PacketHeader { kind, ssrc: 0xDEAD_BEEF, seq, timestamp: 12_345 }
    }

    #[test]
    fn headers_round_trip() {
        let h = header(MediaKind::Voice, 7);
        assert_eq!(PacketHeader::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn foreign_datagrams_are_rejected_before_any_work() {
        assert_eq!(PacketHeader::decode(&[]), Err(MediaError::TooShort(0)));
        assert_eq!(PacketHeader::decode(&[0; 16]), Err(MediaError::BadMagic));

        let mut bytes = header(MediaKind::Voice, 1).encode();
        bytes[2] = 99;
        assert_eq!(PacketHeader::decode(&bytes), Err(MediaError::BadVersion(99)));

        let mut bytes = header(MediaKind::Voice, 1).encode();
        bytes[3] = 77;
        assert_eq!(PacketHeader::decode(&bytes), Err(MediaError::BadKind(77)));
    }

    #[test]
    fn a_sealed_packet_opens_to_what_went_in() {
        let key = SessionKey::random();
        let h = header(MediaKind::Voice, 1);
        let mut out = Vec::new();
        key.seal(h, b"opus frame", &mut out).unwrap();

        assert_eq!(out.len(), HEADER_LEN + b"opus frame".len() + TAG_LEN);
        let (got, plain) = key.open(&out).unwrap();
        assert_eq!(got, h);
        assert_eq!(plain, b"opus frame");
    }

    #[test]
    fn another_key_cannot_open_it() {
        let mut out = Vec::new();
        SessionKey::random().seal(header(MediaKind::Voice, 1), b"secret", &mut out).unwrap();
        assert_eq!(SessionKey::random().open(&out), Err(MediaError::NotAuthentic));
    }

    /// The header is readable *and* authenticated. Re-labelling a packet as coming
    /// from somebody else must therefore break it — that is what stops a relay or a
    /// man in the middle from attributing audio to the wrong person.
    #[test]
    fn tampering_with_the_plaintext_header_breaks_the_payload() {
        let key = SessionKey::random();
        let mut out = Vec::new();
        key.seal(header(MediaKind::Voice, 1), b"hello", &mut out).unwrap();

        let mut forged = out.clone();
        forged[4] ^= 0x01; // a different ssrc
        assert_eq!(key.open(&forged), Err(MediaError::NotAuthentic));

        let mut forged = out.clone();
        forged[12] ^= 0x01; // a different timestamp
        assert_eq!(key.open(&forged), Err(MediaError::NotAuthentic));

        let mut forged = out;
        forged[HEADER_LEN] ^= 0x01; // and the payload itself
        assert_eq!(key.open(&forged), Err(MediaError::NotAuthentic));
    }

    /// The nonce rule, pinned. Two packets from the same session must never share
    /// one, and the three fields that separate them are ssrc, seq and kind — the
    /// last of which is easy to forget, because a sender's voice and video streams
    /// count sequence numbers independently and both start at zero.
    #[test]
    fn nonces_are_unique_across_sender_stream_and_packet() {
        let a = PacketHeader { kind: MediaKind::Voice, ssrc: 1, seq: 0, timestamp: 0 };
        let differ_by_seq = PacketHeader { seq: 1, ..a };
        let differ_by_ssrc = PacketHeader { ssrc: 2, ..a };
        let differ_by_kind = PacketHeader { kind: MediaKind::VideoKey, ..a };
        let same_but_later_timestamp = PacketHeader { timestamp: 960, ..a };

        let nonce = |h: PacketHeader| h.nonce().to_vec();
        assert_ne!(nonce(a), nonce(differ_by_seq));
        assert_ne!(nonce(a), nonce(differ_by_ssrc));
        assert_ne!(nonce(a), nonce(differ_by_kind));
        // The timestamp is deliberately *not* part of the nonce: a resent frame
        // would carry the same one, and a silence-suppressing sender skips
        // timestamps entirely. It contributes nothing to uniqueness that seq does
        // not already guarantee.
        assert_eq!(nonce(a), nonce(same_but_later_timestamp));
    }

    #[test]
    fn a_payload_that_would_fragment_is_refused_rather_than_split_silently() {
        let key = SessionKey::random();
        let mut out = Vec::new();
        assert!(key.seal(header(MediaKind::Voice, 1), &vec![0u8; MAX_PAYLOAD], &mut out).is_ok());
        assert_eq!(
            key.seal(header(MediaKind::Voice, 2), &vec![0u8; MAX_PAYLOAD + 1], &mut out),
            Err(MediaError::TooLarge(MAX_PAYLOAD + 1))
        );
    }

    #[test]
    fn sealing_into_a_dirty_buffer_leaves_no_tail() {
        let key = SessionKey::random();
        let mut out = vec![0xAA; MAX_DATAGRAM];
        key.seal(header(MediaKind::Voice, 1), b"short", &mut out).unwrap();
        assert_eq!(out.len(), HEADER_LEN + 5 + TAG_LEN);
        assert_eq!(key.open(&out).unwrap().1, b"short");
    }

    #[test]
    fn registration_needs_the_key_and_the_right_plaintext() {
        let key = SessionKey::random();
        let mut out = Vec::new();
        key.seal(header(MediaKind::Keepalive, 0), REGISTRATION_PLAINTEXT, &mut out).unwrap();
        assert!(key.verify_registration(&out));

        // Right plaintext, wrong key.
        assert!(!SessionKey::random().verify_registration(&out));

        // Right key, wrong plaintext — a voice packet relabelled as a keepalive.
        let mut forged = Vec::new();
        key.seal(header(MediaKind::Keepalive, 1), b"not a registration", &mut forged).unwrap();
        assert!(!key.verify_registration(&forged));
    }

    #[test]
    fn a_key_never_prints_itself() {
        let text = format!("{:?}", SessionKey::random());
        assert_eq!(text, "SessionKey(redacted)");
    }

    #[test]
    fn base64_round_trips_and_rejects_the_wrong_length() {
        let key = SessionKey::random();
        let text = key.to_base64();
        assert_eq!(SessionKey::from_base64(&text).unwrap().bytes, key.bytes);
        assert!(SessionKey::from_base64("short").is_none());
        assert!(SessionKey::from_base64("!!!not base64!!!").is_none());
    }

    #[test]
    fn a_frame_fragments_and_reassembles() {
        let frame: Vec<u8> = (0..5_000u32).map(|i| i as u8).collect();
        let pieces: Vec<_> = fragment(&frame).collect();
        assert_eq!(pieces.len(), frame.len().div_ceil(MAX_VIDEO_CHUNK));
        assert!(pieces.iter().all(|(_, count, _)| *count as usize == pieces.len()));

        let mut rebuilt = Vec::new();
        for (index, count, chunk) in pieces {
            let mut payload = Vec::new();
            write_fragment(index, count, chunk, &mut payload);
            assert!(payload.len() <= MAX_PAYLOAD);
            let (got_index, got_count, data) = read_fragment(&payload).unwrap();
            assert_eq!((got_index, got_count as usize), (index, frame.len().div_ceil(MAX_VIDEO_CHUNK)));
            rebuilt.extend_from_slice(data);
        }
        assert_eq!(rebuilt, frame);
    }

    #[test]
    fn an_empty_frame_costs_no_packets() {
        assert_eq!(fragment(&[]).count(), 0);
    }

    #[test]
    fn a_nonsensical_fragment_header_is_rejected() {
        assert!(read_fragment(&[0, 0]).is_none(), "too short for a header");
        assert!(read_fragment(&[0, 0, 0, 0]).is_none(), "count of zero");
        assert!(read_fragment(&[0, 5, 0, 3]).is_none(), "fragment 5 of 3");
        assert_eq!(read_fragment(&[0, 0, 0, 1, 9]).unwrap(), (0, 1, &[9u8][..]));
    }

    #[test]
    fn a_voice_frame_is_960_samples() {
        assert_eq!(VOICE_FRAME_SAMPLES, 960);
    }

    #[test]
    fn screen_kinds_are_the_ones_that_need_a_subscription() {
        assert!(MediaKind::VideoKey.is_screen());
        assert!(MediaKind::VideoDelta.is_screen());
        assert!(MediaKind::DesktopAudio.is_screen());
        assert!(!MediaKind::Voice.is_screen());
        assert!(!MediaKind::Keepalive.is_screen());
        assert!(MediaKind::VideoKey.is_video());
        assert!(!MediaKind::DesktopAudio.is_video());
    }
}
