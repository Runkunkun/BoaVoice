//! Voice: capture, clean up, encode, send — and receive, decode, mix, play.
//!
//! The pipeline in one line each, because the order is the design:
//!
//! ```text
//! microphone → gain → denoise → gate → Opus → seal → UDP
//!     UDP → open → jitter buffer → Opus → per-person volume → mix → speakers
//! ```
//!
//! Everything in this module runs on threads that are not the interface's, and two of them have hard
//! deadlines: the capture and playback callbacks. A callback that takes longer than the buffer it was
//! handed produces an audible click, and one that blocks — on a lock the interface holds, on an
//! allocation, on a socket — produces a gap. So the rules here are strict and stated once:
//!
//! * **No allocation in a callback.** Every buffer is sized at startup.
//! * **No lock a slow thread can hold.** The callbacks exchange data with the rest of the app through
//!   lock-free queues and atomics, never through a mutex the interface also takes.
//! * **No I/O in a callback.** Sending is done by a separate thread that the capture callback hands
//!   frames to.

pub mod denoise;
pub mod devices;
pub mod pipeline;
pub mod resample;
pub mod ring;

pub use pipeline::{Status, VoiceSession, MAX_SPEAKERS};
