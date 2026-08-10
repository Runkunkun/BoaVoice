//! A lock-free ring buffer of samples, for handing audio between a callback and a thread.
//!
//! This exists because of one rule: **an audio callback must not wait for anything.** It runs on a
//! thread the operating system schedules in real time and hands a buffer that must be filled before
//! a deadline of a few milliseconds. If it blocks on a mutex that a normal thread holds — and a
//! normal thread can be descheduled at any moment, or be busy decoding an image — the callback
//! misses its deadline and the result is an audible click. Not a slow frame: a click, in a
//! conversation.
//!
//! So the queue between the capture callback and the encoder, and between the network and the
//! playback callback, is this: two atomic indices over a fixed array, no allocation, no locks, no
//! syscalls.
//!
//! The implementation stores samples as `AtomicU32` holding `f32` bits, which makes the whole thing
//! ordinary safe Rust. The usual approach — `UnsafeCell<[f32]>` with the same two indices — is a
//! little faster and needs an unsafe block plus an argument about why it is sound. At 48 000 samples
//! a second, the atomic load and store are not measurable next to the cost of Opus, so the safe
//! version wins on the only axis that matters here.
//!
//! **The discipline it relies on:** one producer and one consumer, each on its own thread. Two
//! producers would corrupt the write index. Nothing in the type prevents that, so it is stated
//! here and each [`Ring`] in the pipeline has exactly one of each by construction.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub struct Ring {
    cells: Box<[AtomicU32]>,
    /// Next index the consumer will read. Only the consumer advances it.
    read: AtomicUsize,
    /// Next index the producer will write. Only the producer advances it.
    write: AtomicUsize,
    /// `capacity - 1`, for wrapping without a division.
    mask: usize,
}

impl Ring {
    /// A ring holding at least `wanted` samples.
    ///
    /// Rounded up to a power of two so that wrapping is a mask rather than a modulo — and one slot
    /// is always left empty, because a full buffer and an empty one would otherwise have the same
    /// pair of indices.
    pub fn new(wanted: usize) -> Ring {
        let capacity = wanted.next_power_of_two().max(2);
        Ring {
            cells: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            mask: capacity - 1,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cells.len() - 1
    }

    /// How many samples are waiting.
    pub fn len(&self) -> usize {
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        write.wrapping_sub(read) & self.mask
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Push what fits. Returns how many samples were taken.
    ///
    /// **Producer side only.** Dropping the overflow rather than blocking is the right failure here:
    /// if the consumer has stopped keeping up, the *newest* audio is what matters and waiting would
    /// turn a dropped frame into a missed deadline.
    pub fn push(&self, samples: &[f32]) -> usize {
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let free = self.capacity() - (write.wrapping_sub(read) & self.mask);
        let taken = samples.len().min(free);
        for (i, sample) in samples[..taken].iter().enumerate() {
            self.cells[(write + i) & self.mask].store(sample.to_bits(), Ordering::Relaxed);
        }
        // Release, so a consumer that sees the new index also sees the samples written above.
        self.write.store((write + taken) & self.mask, Ordering::Release);
        taken
    }

    /// Fill `out` with what is available. Returns how many samples were written; the rest of `out`
    /// is left alone.
    ///
    /// **Consumer side only.**
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let available = write.wrapping_sub(read) & self.mask;
        let taken = out.len().min(available);
        for (i, slot) in out[..taken].iter_mut().enumerate() {
            *slot = f32::from_bits(self.cells[(read + i) & self.mask].load(Ordering::Relaxed));
        }
        self.read.store((read + taken) & self.mask, Ordering::Release);
        taken
    }

    /// Discard everything waiting.
    ///
    /// Consumer side. Used when a stream is re-primed after an underrun: the samples still in the
    /// buffer are older than the gap that just happened, and playing them would add the delay
    /// rather than skipping it.
    pub fn clear(&self) {
        self.read.store(self.write.load(Ordering::Acquire), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_come_back_in_order() {
        let ring = Ring::new(8);
        assert!(ring.is_empty());
        assert_eq!(ring.push(&[1.0, 2.0, 3.0]), 3);
        assert_eq!(ring.len(), 3);

        let mut out = [0.0; 3];
        assert_eq!(ring.pop(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        assert!(ring.is_empty());
    }

    #[test]
    fn the_capacity_is_a_power_of_two_with_one_slot_reserved() {
        // The reserved slot is what makes "full" and "empty" distinguishable.
        assert_eq!(Ring::new(5).capacity(), 7);
        assert_eq!(Ring::new(8).capacity(), 7);
        assert_eq!(Ring::new(9).capacity(), 15);
        assert_eq!(Ring::new(0).capacity(), 1);
    }

    /// The failure mode that matters: when the consumer has stopped keeping up, pushing must return
    /// rather than block, and it must not corrupt what is already there.
    #[test]
    fn a_full_ring_drops_the_overflow_instead_of_blocking() {
        let ring = Ring::new(4);
        let capacity = ring.capacity();
        let taken = ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(taken, capacity);
        assert_eq!(ring.len(), capacity);
        assert_eq!(ring.push(&[9.0]), 0, "nothing fits");

        let mut out = vec![0.0; capacity];
        assert_eq!(ring.pop(&mut out), capacity);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn popping_more_than_is_there_leaves_the_rest_of_the_buffer_alone() {
        let ring = Ring::new(8);
        ring.push(&[1.0, 2.0]);
        let mut out = [-1.0; 4];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(out, [1.0, 2.0, -1.0, -1.0], "the tail is the caller's to fill with silence");
    }

    #[test]
    fn indices_wrap_without_losing_anything() {
        let ring = Ring::new(4);
        let mut out = [0.0; 2];
        // Go round the buffer several times; if the masking were wrong this would drift.
        for round in 0..20 {
            let a = round as f32;
            let b = a + 0.5;
            assert_eq!(ring.push(&[a, b]), 2);
            assert_eq!(ring.pop(&mut out), 2);
            assert_eq!(out, [a, b], "round {round}");
        }
    }

    #[test]
    fn clearing_drops_what_is_stale() {
        let ring = Ring::new(8);
        ring.push(&[1.0, 2.0, 3.0]);
        ring.clear();
        assert!(ring.is_empty());
        // And the ring is still usable afterwards.
        ring.push(&[4.0]);
        let mut out = [0.0; 1];
        assert_eq!(ring.pop(&mut out), 1);
        assert_eq!(out, [4.0]);
    }

    /// Two threads, the way the pipeline actually uses it: one pushing, one popping, and every
    /// sample arriving exactly once and in order.
    #[test]
    fn a_producer_and_a_consumer_on_two_threads_lose_nothing() {
        use std::sync::Arc;

        let ring = Arc::new(Ring::new(64));
        const TOTAL: usize = 20_000;

        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                let mut sent = 0usize;
                while sent < TOTAL {
                    let chunk: Vec<f32> = (sent..(sent + 32).min(TOTAL)).map(|i| i as f32).collect();
                    let taken = ring.push(&chunk);
                    sent += taken;
                    if taken < chunk.len() {
                        std::thread::yield_now();
                    }
                }
            })
        };

        let mut received = Vec::with_capacity(TOTAL);
        let mut out = [0.0f32; 16];
        while received.len() < TOTAL {
            let taken = ring.pop(&mut out);
            if taken == 0 {
                std::thread::yield_now();
                continue;
            }
            received.extend_from_slice(&out[..taken]);
        }
        producer.join().unwrap();

        assert_eq!(received.len(), TOTAL);
        for (i, sample) in received.iter().enumerate() {
            assert_eq!(*sample, i as f32, "sample {i} arrived out of order");
        }
    }
}
