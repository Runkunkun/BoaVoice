//! BoaVoice — the client.
//!
//! Voice, screens and chat against a server you host yourself, in the shape of its siblings
//! RedPython and BluePython: eframe over wgpu, a Catppuccin Mocha palette, glass panels over the
//! platform's own blur, and no web view anywhere.
//!
//! The layers, and what each one is not allowed to do:
//!
//! ```text
//! ui/         everything drawn. Returns an Action; never performs one.
//! state       what the server has told us. Only the frame loop writes to it.
//! net/        the network thread. Owns the socket; shares nothing but two channels.
//! audio/      capture, encode, decode, mix. Runs on the audio callbacks' own threads.
//! media/      the UDP socket for voice and video.
//! screen/     capture and encode a display; decode somebody else's.
//! transfer/   direct file transfer, peer to peer, over magic-wormhole.
//! cache       attachments kept locally, permanently, because the server does not.
//! theme       the palette and the roles the UI names.
//! platform/   the one thing egui cannot do portably: real window vibrancy.
//! ```
//!
//! Nothing below `ui/` knows egui exists, which is what lets the network thread, the audio callbacks
//! and the encoder each run on their own schedule and report back through channels.

pub mod audio;
pub mod cache;
pub mod diagnostics;
pub mod media;
pub mod net;
pub mod paths;
pub mod platform;
pub mod screen;
pub mod settings;
pub mod state;
pub mod theme;
pub mod transfer;
pub mod ui;

pub use ui::App;
