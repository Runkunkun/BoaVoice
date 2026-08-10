//! Turning attachment bytes into textures, off the interface thread.
//!
//! Decoding a photograph is tens of milliseconds and allocating its pixels is tens of
//! megabytes. Doing that where the frame is drawn drops frames visibly — and a chat log
//! scrolling past twenty images would do it twenty times in one frame. So decoding happens
//! on a worker and the result arrives as an already-decoded `ColorImage`, which the interface
//! uploads to the GPU in microseconds.
//!
//! Two limits are enforced here rather than left to chance.
//!
//! **Nothing is decoded twice.** A hash that is being worked on is remembered, so a log that
//! draws the same image on every frame for a second does not queue sixty decodes of it.
//!
//! **Large pictures are scaled down first.** A 6000-pixel-wide photo is 144 MB of RGBA, and
//! keeping several of those as textures is how a chat client ends up using two gigabytes of
//! video memory. They are reduced to something no larger than a screen can show.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

/// Longest edge kept when decoding.
///
/// 2560 covers a full-width image on a 5K display; anything bigger is being scaled down for
/// display anyway, so the extra pixels cost memory and buy nothing. Scaling here rather than
/// at draw time also means the *texture* is small, which is where the memory actually goes.
const MAX_EDGE: u32 = 2_560;

/// What the interface has for one attachment.
pub enum Slot {
    /// A decode is in flight.
    Loading,
    /// Ready to draw, with its natural size in points.
    Ready { texture: egui::TextureHandle, size: egui::Vec2 },
    /// The bytes are here and are not a picture this build can decode.
    Undecodable(String),
}

/// The decoder: a worker thread, a queue, and everything already decoded.
pub struct Images {
    requests: Sender<Job>,
    results: Receiver<Done>,
    slots: HashMap<String, Slot>,
}

struct Job {
    sha256: String,
    bytes: Vec<u8>,
}

struct Done {
    sha256: String,
    result: Result<egui::ColorImage, String>,
}

impl Images {
    pub fn new() -> Self {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Job>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<Done>();

        // One worker, not a pool. Decodes are already off the frame path, and doing several at
        // once mostly competes for memory bandwidth — while making the peak footprint the sum
        // of the largest few images rather than just the largest one.
        let builder = std::thread::Builder::new().name("boa-images".into());
        if let Err(err) = builder.spawn(move || {
            while let Ok(job) = request_rx.recv() {
                let result = decode(&job.bytes).map_err(|err| err.to_string());
                if result_tx.send(Done { sha256: job.sha256, result }).is_err() {
                    return;
                }
            }
        }) {
            log::error!("images: could not start the decoder: {err}");
        }

        Images { requests: request_tx, results: result_rx, slots: HashMap::new() }
    }

    /// Take in whatever the worker finished. Call once per frame, before drawing.
    pub fn collect(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.results.try_recv() {
            let slot = match done.result {
                Ok(image) => {
                    let size = egui::vec2(image.width() as f32, image.height() as f32);
                    // `Linear` filtering: a chat image is almost always drawn smaller than it
                    // is, and nearest-neighbour downscaling of a photograph looks broken.
                    let texture = ctx.load_texture(
                        format!("attachment-{}", done.sha256),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    Slot::Ready { texture, size }
                }
                Err(err) => {
                    log::debug!("images: {}: {err}", done.sha256);
                    Slot::Undecodable(err)
                }
            };
            self.slots.insert(done.sha256, slot);
        }
    }

    /// What we have for this hash, queueing a decode if the bytes are on disk and nothing has
    /// started yet.
    pub fn get(&mut self, sha256: &str) -> Option<&Slot> {
        if !self.slots.contains_key(sha256) {
            // Only if the bytes are actually here. Fetching them is the network layer's job,
            // and asking for a decode of a file that has not arrived would spin.
            if crate::cache::have(sha256) {
                match crate::cache::read(sha256) {
                    Ok(bytes) => {
                        self.slots.insert(sha256.to_string(), Slot::Loading);
                        let _ = self.requests.send(Job { sha256: sha256.to_string(), bytes });
                    }
                    Err(err) => {
                        self.slots.insert(sha256.to_string(), Slot::Undecodable(err.to_string()));
                    }
                }
            }
        }
        self.slots.get(sha256)
    }

    /// Forget a decoded image, so the next look re-reads it.
    ///
    /// Used when a decode failed and the bytes have since been replaced — a retry after a
    /// truncated download, which would otherwise be stuck showing the failure forever.
    pub fn forget(&mut self, sha256: &str) {
        self.slots.remove(sha256);
    }

    /// How many textures are live, for the settings screen.
    pub fn loaded(&self) -> usize {
        self.slots.values().filter(|slot| matches!(slot, Slot::Ready { .. })).count()
    }
}

impl Default for Images {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode and, if necessary, shrink.
fn decode(bytes: &[u8]) -> anyhow::Result<egui::ColorImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let image = reader.decode()?;

    let (width, height) = (image.width(), image.height());
    let image = if width.max(height) > MAX_EDGE {
        let scale = MAX_EDGE as f32 / width.max(height) as f32;
        // `Triangle` rather than `Lanczos3`: this runs on a background thread but still on
        // somebody's laptop, and the difference on a photograph being shrunk to fit a chat
        // column is not visible while the cost is several times higher.
        image.resize(
            ((width as f32 * scale) as u32).max(1),
            ((height as f32 * scale) as u32).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };

    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// How big to draw an image inside `available`, keeping its proportions.
///
/// Never larger than its natural size — an image blown up past its pixels looks worse than one
/// shown small, and in a chat log the small one is what the sender saw when they posted it.
pub fn fit(natural: egui::Vec2, available: f32, max_height: f32) -> egui::Vec2 {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let scale = (available / natural.x).min(max_height / natural.y).min(1.0);
    natural * scale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 2x1 PNG, encoded rather than written out by hand.
    ///
    /// The hand-written version of this had a wrong CRC in its header, which the decoder
    /// rejected — an hour of confusion for a test fixture. Encoding with the same library that
    /// will decode it cannot be wrong in that way, and the shape is deliberately not square so
    /// that a transposed width and height would show up.
    fn png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 1, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn a_png_decodes_to_its_own_size() {
        let image = decode(&png()).unwrap();
        assert_eq!((image.width(), image.height()), (2, 1));
    }

    #[test]
    fn rubbish_is_an_error_rather_than_a_panic() {
        assert!(decode(b"not an image").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn fitting_never_enlarges_and_keeps_the_proportions() {
        let natural = egui::vec2(800.0, 400.0);

        // Narrower than the image: scaled down, aspect kept.
        let fitted = fit(natural, 400.0, 1_000.0);
        assert_eq!(fitted, egui::vec2(400.0, 200.0));

        // Wider than the image: shown at its own size, not stretched.
        assert_eq!(fit(natural, 2_000.0, 1_000.0), natural);

        // A height limit wins when it is the tighter one.
        assert_eq!(fit(natural, 2_000.0, 100.0), egui::vec2(200.0, 100.0));

        // And a degenerate size does not divide by zero.
        assert_eq!(fit(egui::vec2(0.0, 0.0), 100.0, 100.0), egui::Vec2::ZERO);
    }
}
