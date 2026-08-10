//! The watching side: fragments back into pictures, pictures into a texture.
//!
//! Two rules shape this, and both are about what to do with a stream that has holes in it — which
//! over UDP is the normal case rather than the exception.
//!
//! **An incomplete picture is dropped, not waited for.** A frame is only useful for the fraction of a
//! second it belongs to; holding one back while its missing fragment might arrive delays every frame
//! behind it too. So a picture that is not complete when the next one starts is abandoned.
//!
//! **Nothing is decoded until a keyframe has arrived.** A delta frame decoded without its reference is
//! not "slightly wrong" — it is the smear of green blocks everybody recognises, and it persists until
//! the next keyframe because each subsequent frame builds on the broken one. Waiting costs at most
//! the keyframe interval, and it is the difference between "the picture appears in a moment" and "the
//! picture is broken".
//!
//! # Where the queue goes, and why it matters
//!
//! Fragments are reassembled **in the network thread**, and the queue between it and the decoder
//! carries whole pictures. Putting the queue the other way round — fragments in, reassembly in the
//! decoder — reads as the same thing and is not, because the two ends behave completely differently
//! when the queue overflows:
//!
//! * A queue of **fragments** that overflows loses arbitrary pieces of whichever pictures happened to
//!   be arriving. Every picture missing a piece is a picture that never completes. And a keyframe is
//!   *large* — measured at 115 KB, a hundred datagrams, for a 1080p share — so a queue deep enough for
//!   several delta frames is not deep enough for one keyframe. Once the decoder falls a fraction of a
//!   second behind, no keyframe can ever be completed again, and the picture is gone for good.
//! * A queue of **pictures** that overflows drops whole pictures, which is exactly what should happen:
//!   the stream stays decodable, and a decoder that cannot keep up simply shows fewer frames.
//!
//! This was not a theoretical concern. It is the bug that made a share stop after the first few frames
//! and never recover.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use boa_proto::media::{read_fragment, MediaKind, PacketHeader};
// `dimensions` and `write_rgba8` come from this trait rather than from the decoded frame itself.
use openh264::formats::YUVSource as _;

/// How many pictures may be in flight before the oldest incomplete one is given up on.
///
/// Two. Fragments of one picture arrive together; a picture still missing pieces once two newer ones
/// have started is not going to be completed.
const IN_FLIGHT: usize = 2;

/// How many whole pictures may wait for the decoder.
///
/// Four. Enough that a decoder which stalls for a frame catches up; few enough that one which stalls
/// for a second does not accumulate a backlog it would then show late. Overflow drops the *newest*
/// arrival, which is the wrong one to lose in theory and the right one in practice: dropping the oldest
/// would mean re-ordering a stream whose pictures must be decoded in order.
const QUEUE: usize = 4;

/// One picture on its way to the decoder: whether it is a keyframe, and its bytes.
type Picture = (bool, Vec<u8>);

/// Where a decoder gets its pictures — always whole ones, from one of two places.
///
/// Watching somebody else means pictures reassembled from UDP fragments by [`Tap`], which may be
/// missing some. Watching **your own** share means pictures straight from the encoder, before they are
/// cut up: nothing can be lost, and there is no relay in between — the relay never sends a stream back
/// to whoever sent it, so a local preview is the only way to see what everybody else is seeing.
pub struct Feed(std::sync::mpsc::Receiver<Picture>);

/// The network thread's end of a share being watched.
///
/// Holds the reassembly state, so a fragment costs one `memcpy` on that thread and the decoder is
/// handed something it can always decode. Lives behind the media session's lock, which is why it is a
/// type of its own rather than a bare channel.
pub struct Tap {
    pictures: std::sync::mpsc::SyncSender<Picture>,
    assembler: Reassembler,
    /// Whose stream this is. The relay only forwards streams we subscribed to, but a share that has
    /// just been swapped for another can still have packets in flight.
    ssrc: u32,
    /// Pictures that never arrived whole, plus those dropped because the decoder was behind. Shown in
    /// the interface: "it stopped" and "you are losing a third of the packets" want different answers.
    dropped: Arc<AtomicU64>,
}

impl Tap {
    /// The two ends of a watch without a decoder between them.
    ///
    /// Separated out because it is the seam the tests need: a decoder thread drains the queue as fast as
    /// it can, which is exactly what makes an overflow bug invisible.
    fn pair(
        ssrc: u32,
        depth: usize,
    ) -> (Tap, std::sync::mpsc::Receiver<Picture>, Arc<AtomicU64>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(depth);
        let dropped = Arc::new(AtomicU64::new(0));
        let tap =
            Tap { pictures: tx, assembler: Reassembler::default(), ssrc, dropped: dropped.clone() };
        (tap, rx, dropped)
    }

    /// Take one media packet. Complete pictures go to the decoder; the rest is bookkeeping.
    pub fn feed(&mut self, header: &PacketHeader, payload: &[u8]) {
        if header.ssrc != self.ssrc || !header.kind.is_video() {
            return;
        }
        let Some(picture) = self.assembler.feed(header, payload, &self.dropped) else { return };
        let keyframe = header.kind == MediaKind::VideoKey;
        // `try_send`, because this is the network thread: blocking here would stall voice as well, and
        // a picture the decoder has no room for is a picture already out of date.
        if self.pictures.try_send((keyframe, picture)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A decoded picture, ready to become a texture.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Tightly packed RGBA.
    pub rgba: Vec<u8>,
    /// Increments with each decoded picture, so the interface can tell a new frame from the one it
    /// already uploaded without comparing pixels.
    pub generation: u64,
}

/// Watches one person's screen: a decoder thread and the latest picture it produced.
pub struct Watcher {
    latest: Arc<Mutex<Option<Frame>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub frames: Arc<AtomicU64>,
    pub dropped: Arc<AtomicU64>,
    /// Pictures the decoder *refused*, which is a different fault from a picture that never arrived
    /// whole. Loss means the network or a slow decoder; a refusal means the bytes were wrong — a
    /// fragment stitched into the wrong place, a parameter set missing. One is weather, the other is a
    /// bug, and counting them together hides the bug.
    pub failed: Arc<AtomicU64>,
    /// Set once a keyframe has been decoded, so the interface can say "waiting for a keyframe"
    /// rather than showing an empty box.
    pub started: Arc<AtomicBool>,
    /// When watching began. After a few seconds of nothing at all, "waiting for a keyframe" is the
    /// wrong thing to say — the packets are not arriving, and that has a different cause and a
    /// different fix.
    pub since: std::time::Instant,
}

impl Watcher {
    /// Start watching somebody's share: a [`Tap`] for the network thread and a decoder for the picture.
    ///
    /// The two halves share the dropped counter, because the drop can happen at either end — a fragment
    /// that never arrived, or a picture the decoder had no room for — and to somebody watching a frozen
    /// picture they mean the same thing.
    pub fn start(ssrc: u32) -> (Tap, Watcher) {
        let (tap, pictures, dropped) = Tap::pair(ssrc, QUEUE);
        let watcher = Watcher::feed(Feed(pictures), dropped);
        (tap, watcher)
    }

    /// Decode this machine's own share, for a preview of what is going out.
    pub fn preview(pictures: std::sync::mpsc::Receiver<Picture>) -> Watcher {
        Watcher::feed(Feed(pictures), Arc::new(AtomicU64::new(0)))
    }

    fn feed(source: Feed, dropped: Arc<AtomicU64>) -> Watcher {
        let failed = Arc::new(AtomicU64::new(0));
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let started = Arc::new(AtomicBool::new(false));

        let thread = {
            let latest = latest.clone();
            let stop = stop.clone();
            let frames = frames.clone();
            let dropped = dropped.clone();
            let started = started.clone();
            std::thread::Builder::new()
                .name("boa-screen-rx".into())
                .spawn(move || {
                    if let Err(err) = decode_loop(source, &latest, &stop, &frames, &dropped, &started)
                    {
                        log::error!("screen: decoding stopped: {err:#}");
                        crate::diagnostics::note(&format!("screen: decode stopped: {err:#}"));
                    }
                })
                .expect("spawning a thread")
        };

        Watcher {
            latest,
            stop,
            thread: Some(thread),
            frames,
            dropped,
            failed,
            started,
            since: std::time::Instant::now(),
        }
    }

    /// Take the latest picture, if there is one newer than `seen`.
    ///
    /// Takes rather than clones: the interface uploads it to the GPU immediately and a copy of a
    /// 4K frame is 33 MB.
    pub fn take_frame(&self, seen: u64) -> Option<Frame> {
        let mut latest = self.latest.lock().ok()?;
        match latest.as_ref() {
            Some(frame) if frame.generation > seen => latest.take(),
            _ => None,
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // The thread is blocked on the channel; dropping the sender in the media thread is what
            // wakes it. Joining without that would hang, so this is a detach in all but name — the
            // handle is taken so the drop does not wait.
            drop(thread);
        }
    }
}

fn decode_loop(
    source: Feed,
    latest: &Mutex<Option<Frame>>,
    stop: &AtomicBool,
    frames: &AtomicU64,
    failed: &AtomicU64,
    started: &AtomicBool,
) -> Result<()> {
    let mut decoder = openh264::decoder::Decoder::new().context("starting the H.264 decoder")?;
    let mut generation = 0u64;

    while !stop.load(Ordering::Acquire) {
        // Blocking: the thread has nothing else to do, and the channel closing is how it learns the
        // share has ended.
        let Ok((keyframe, picture)) = source.0.recv() else { return Ok(()) };

        // Until a keyframe has been seen, a delta has no reference and decoding it produces the
        // familiar smear rather than a picture.
        if !started.load(Ordering::Relaxed) {
            if !keyframe {
                continue;
            }
            started.store(true, Ordering::Release);
        }

        match decoder.decode(&picture) {
            Ok(Some(decoded)) => {
                let (width, height) = decoded.dimensions();
                let mut rgba = vec![0u8; width * height * 4];
                decoded.write_rgba8(&mut rgba);
                generation += 1;
                frames.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut slot) = latest.lock() {
                    // Replacing rather than queueing: if the interface has not drawn the previous
                    // frame yet, the previous frame is already out of date.
                    *slot = Some(Frame { width, height, rgba, generation });
                }
            }
            // A picture that produced nothing is normal — the decoder is waiting for the rest of an
            // access unit — and is not worth a log line per frame.
            Ok(None) => {}
            Err(err) => {
                log::debug!("screen: decode: {err}");
                failed.fetch_add(1, Ordering::Relaxed);
                // A failed decode means the reference chain is broken; wait for the next keyframe
                // rather than feeding it more deltas it cannot use.
                started.store(false, Ordering::Release);
            }
        }
    }
    Ok(())
}

/// Collects fragments into complete pictures.
#[derive(Default)]
pub struct Reassembler {
    /// Keyed by the timestamp every fragment of one picture shares.
    building: HashMap<u32, Partial>,
    /// The timestamp of the newest picture seen, so older ones can be given up on.
    newest: Option<u32>,
}

struct Partial {
    /// One slot per fragment; `None` until it arrives.
    pieces: Vec<Option<Vec<u8>>>,
    have: usize,
}

impl Reassembler {
    /// Add a fragment. Returns the picture when this was the last piece.
    pub fn feed(
        &mut self,
        header: &PacketHeader,
        payload: &[u8],
        dropped: &AtomicU64,
    ) -> Option<Vec<u8>> {
        let (index, count, data) = read_fragment(payload)?;

        // A single-fragment picture is the common case for a delta frame on a still screen, and it
        // does not need the map at all.
        if count == 1 {
            self.note_newest(header.timestamp, dropped);
            return Some(data.to_vec());
        }

        let partial = self.building.entry(header.timestamp).or_insert_with(|| Partial {
            pieces: (0..count as usize).map(|_| None).collect(),
            have: 0,
        });
        // A fragment claiming a different count for the same picture is corrupt; the whole picture
        // goes rather than risking a mixed-up frame.
        if partial.pieces.len() != count as usize {
            self.building.remove(&header.timestamp);
            dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if partial.pieces[index as usize].is_none() {
            partial.pieces[index as usize] = Some(data.to_vec());
            partial.have += 1;
        }

        let complete = partial.have == partial.pieces.len();
        self.note_newest(header.timestamp, dropped);

        if !complete {
            return None;
        }
        let partial = self.building.remove(&header.timestamp)?;
        let mut picture = Vec::with_capacity(partial.pieces.iter().flatten().map(Vec::len).sum());
        for piece in partial.pieces.into_iter().flatten() {
            picture.extend_from_slice(&piece);
        }
        Some(picture)
    }

    /// Record the newest picture and abandon any that are too far behind it.
    fn note_newest(&mut self, timestamp: u32, dropped: &AtomicU64) {
        let newer = self.newest.is_none_or(|newest| timestamp.wrapping_sub(newest) < u32::MAX / 2);
        if newer {
            self.newest = Some(timestamp);
        }
        let Some(newest) = self.newest else { return };

        if self.building.len() > IN_FLIGHT {
            // Anything older than the two most recent is not going to be completed.
            let mut timestamps: Vec<u32> = self.building.keys().copied().collect();
            timestamps.sort_by_key(|t| newest.wrapping_sub(*t));
            for stale in timestamps.into_iter().skip(IN_FLIGHT) {
                self.building.remove(&stale);
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// How many pictures are part-assembled, for tests and diagnostics.
    pub fn in_flight(&self) -> usize {
        self.building.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_proto::media::{fragment, write_fragment, MAX_VIDEO_CHUNK};

    fn header(timestamp: u32, seq: u32) -> PacketHeader {
        PacketHeader { kind: MediaKind::VideoKey, ssrc: 1, seq, timestamp }
    }

    /// **The bug this file's header is about.** A keyframe is far bigger than the queue is deep, and
    /// with the queue on the wrong side of the reassembler that made it impossible to assemble one:
    /// measured at 102 datagrams for a 1080p share against a queue of 120 packets, so the first stall
    /// killed the picture permanently. Reassembled here, a picture of any size is one queue slot.
    #[test]
    fn a_keyframe_far_larger_than_the_queue_still_arrives_whole() {
        // 200 fragments — nearly twice what the old fragment queue could hold, and a realistic size for
        // a keyframe of a busy 1080p screen.
        let picture: Vec<u8> = (0..200 * MAX_VIDEO_CHUNK as u32).map(|i| i as u8).collect();
        let (mut tap, pictures, dropped) = Tap::pair(7, QUEUE);

        let payloads = packets(&picture);
        assert!(payloads.len() > QUEUE * 10, "the test needs a picture much larger than the queue");
        for (i, payload) in payloads.iter().enumerate() {
            tap.feed(&PacketHeader { kind: MediaKind::VideoKey, ssrc: 7, seq: i as u32, timestamp: 1 }, payload);
        }

        let (keyframe, arrived) = pictures.try_recv().expect("one whole picture");
        assert!(keyframe);
        assert_eq!(arrived, picture, "the picture came back changed");
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    /// And when the decoder cannot keep up, what is lost is whole pictures rather than pieces of them.
    /// The distinction is the whole point: a queue that sheds fragments corrupts *every* picture,
    /// including the keyframes a stream needs to recover.
    #[test]
    fn an_overflowing_queue_loses_whole_pictures_not_pieces_of_them() {
        let (mut tap, pictures, dropped) = Tap::pair(7, QUEUE);

        // Nothing drains the queue, so everything past its depth has to go somewhere.
        let sent = QUEUE + 5;
        for n in 0..sent {
            let picture: Vec<u8> = (0..3_000u32).map(|i| (i as u8).wrapping_add(n as u8)).collect();
            for (i, payload) in packets(&picture).iter().enumerate() {
                let header = PacketHeader {
                    kind: MediaKind::VideoDelta,
                    ssrc: 7,
                    seq: (n * 10 + i) as u32,
                    timestamp: n as u32 + 1,
                };
                tap.feed(&header, payload);
            }
        }

        // Whatever is in the queue is intact: every picture is the length it was sent at, and its bytes
        // are the ones its sender produced.
        let mut received = 0;
        while let Ok((_, picture)) = pictures.try_recv() {
            assert_eq!(picture.len(), 3_000, "a picture arrived in pieces");
            let n = picture[0];
            let expected: Vec<u8> = (0..3_000u32).map(|i| (i as u8).wrapping_add(n)).collect();
            assert_eq!(picture, expected, "a picture arrived mixed up with another");
            received += 1;
        }
        assert_eq!(received, QUEUE, "the queue should be full");
        assert_eq!(
            dropped.load(Ordering::Relaxed) as usize,
            sent - QUEUE,
            "every picture that did not fit should be counted"
        );
    }

    /// Fragment a picture the way the sender does, and return the payloads.
    fn packets(picture: &[u8]) -> Vec<Vec<u8>> {
        fragment(picture)
            .map(|(index, count, chunk)| {
                let mut payload = Vec::new();
                write_fragment(index, count, chunk, &mut payload);
                payload
            })
            .collect()
    }

    #[test]
    fn a_picture_survives_being_cut_up_and_put_back() {
        let picture: Vec<u8> = (0..7_000u32).map(|i| i as u8).collect();
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);

        let payloads = packets(&picture);
        assert!(payloads.len() > 3, "the test needs a multi-fragment picture");

        let mut out = None;
        for (i, payload) in payloads.iter().enumerate() {
            out = assembler.feed(&header(100, i as u32), payload, &dropped);
        }
        assert_eq!(out.unwrap(), picture);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(assembler.in_flight(), 0, "a completed picture leaves nothing behind");
    }

    /// UDP reorders. A picture whose fragments arrive backwards is still a picture.
    #[test]
    fn fragments_may_arrive_in_any_order() {
        let picture: Vec<u8> = (0..5_000u32).map(|i| (i * 7) as u8).collect();
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);

        let mut payloads = packets(&picture);
        payloads.reverse();
        let mut out = None;
        for (i, payload) in payloads.iter().enumerate() {
            out = assembler.feed(&header(100, i as u32), payload, &dropped);
        }
        assert_eq!(out.unwrap(), picture);
    }

    #[test]
    fn a_single_fragment_picture_needs_no_bookkeeping() {
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);
        let payloads = packets(b"one small delta frame");
        assert_eq!(payloads.len(), 1);
        assert_eq!(
            assembler.feed(&header(1, 0), &payloads[0], &dropped).unwrap(),
            b"one small delta frame"
        );
        assert_eq!(assembler.in_flight(), 0);
    }

    #[test]
    fn a_duplicate_fragment_does_not_complete_a_picture_early() {
        let picture = vec![7u8; MAX_VIDEO_CHUNK * 3];
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);
        let payloads = packets(&picture);

        // The same fragment three times must not look like three fragments.
        for _ in 0..3 {
            assert!(assembler.feed(&header(5, 0), &payloads[0], &dropped).is_none());
        }
        assert!(assembler.feed(&header(5, 1), &payloads[1], &dropped).is_none());
        assert!(assembler.feed(&header(5, 2), &payloads[2], &dropped).is_some());
    }

    /// The rule that keeps latency bounded: a picture missing a fragment is abandoned once newer
    /// pictures have started, rather than held in the hope that the fragment turns up.
    #[test]
    fn an_incomplete_picture_is_abandoned_once_newer_ones_arrive() {
        let picture = vec![3u8; MAX_VIDEO_CHUNK * 3];
        let payloads = packets(&picture);
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);

        // Picture 1 loses a fragment.
        assembler.feed(&header(1, 0), &payloads[0], &dropped);
        // Three more pictures start.
        for timestamp in 2..=4 {
            assembler.feed(&header(timestamp, timestamp), &payloads[0], &dropped);
        }
        assert!(assembler.in_flight() <= IN_FLIGHT, "{}", assembler.in_flight());
        assert!(dropped.load(Ordering::Relaxed) >= 1, "the abandoned picture should be counted");
    }

    #[test]
    fn a_fragment_that_contradicts_its_picture_discards_it() {
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);

        // Fragment 0 of 3, then a fragment claiming 0 of 2 for the same picture.
        let mut three = Vec::new();
        write_fragment(0, 3, b"aaa", &mut three);
        let mut two = Vec::new();
        write_fragment(0, 2, b"bbb", &mut two);

        assert!(assembler.feed(&header(9, 0), &three, &dropped).is_none());
        assert!(assembler.feed(&header(9, 1), &two, &dropped).is_none());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(assembler.in_flight(), 0);
    }

    #[test]
    fn a_nonsensical_payload_is_ignored() {
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);
        assert!(assembler.feed(&header(1, 0), &[0, 0], &dropped).is_none(), "too short");
        assert!(assembler.feed(&header(1, 0), &[0, 5, 0, 3], &dropped).is_none(), "5 of 3");
    }

    /// The whole path, with a real encoder and a real decoder: encode two frames, fragment them the
    /// way the sender does, reassemble them the way the watcher does, and decode. If the packet
    /// format and the NAL grouping disagree in any way, this is where it shows.
    #[test]
    fn an_encoded_frame_survives_the_wire_and_decodes() {
        use openh264::encoder::Encoder;
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        use openh264::OpenH264API;

        const W: usize = 160;
        const H: usize = 96;

        let api = OpenH264API::from_source();
        let mut encoder = Encoder::with_api_config(api, openh264::encoder::EncoderConfig::new())
            .expect("an encoder");

        // Two frames of a moving gradient, so the second one is a real delta rather than a copy.
        let mut assembler = Reassembler::default();
        let dropped = AtomicU64::new(0);
        let mut decoder = openh264::decoder::Decoder::new().expect("a decoder");
        let mut decoded_any = false;

        for step in 0..2u32 {
            let mut rgb = vec![0u8; W * H * 3];
            for y in 0..H {
                for x in 0..W {
                    let i = (y * W + x) * 3;
                    rgb[i] = ((x + step as usize * 20) % 256) as u8;
                    rgb[i + 1] = (y % 256) as u8;
                    rgb[i + 2] = 128;
                }
            }
            let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (W, H)));
            let bitstream = encoder.encode(&yuv).expect("encoding");
            let annexb = bitstream.to_vec();

            // Exactly what the sender does with it.
            let mut rebuilt = None;
            for (index, count, chunk) in fragment(&annexb) {
                let mut payload = Vec::new();
                write_fragment(index, count, chunk, &mut payload);
                assert!(payload.len() <= boa_proto::media::MAX_PAYLOAD);
                rebuilt = assembler.feed(&header(step * 33, index as u32), &payload, &dropped);
            }
            let rebuilt = rebuilt.expect("the picture should be complete");
            assert_eq!(rebuilt, annexb, "the bytes must survive fragmentation exactly");

            if let Ok(Some(picture)) = decoder.decode(&rebuilt) {
                let (width, height) = picture.dimensions();
                assert_eq!((width, height), (W, H));
                let mut rgba = vec![0u8; width * height * 4];
                picture.write_rgba8(&mut rgba);
                assert!(rgba.iter().any(|byte| *byte != 0), "the frame decoded to nothing");
                decoded_any = true;
            }
        }
        assert!(decoded_any, "neither frame decoded");
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }
}
