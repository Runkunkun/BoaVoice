//! The capture itself: `SCStream` in, Annex-B pictures out.
//!
//! ```text
//!  SCStream ──▶ CVPixelBuffer ──▶ VideoToolbox ──▶ Annex-B ──▶ channel ──▶ fragment ──▶ UDP
//! ```
//!
//! Two design decisions worth stating, because both are the difference between a stream that works and
//! one that is subtly late.
//!
//! **The frame is encoded on the callback's own queue.** ScreenCaptureKit hands over a sample buffer on
//! a serial queue and expects to be given it back quickly. Handing it to the encoder is quick:
//! `VTCompressionSessionEncodeFrame` queues the frame and returns, doing the work on VideoToolbox's own
//! threads. So there is no thread of our own here at all, and no copy of the picture — the pixel buffer
//! goes straight from the window server to the encoder.
//!
//! **The finished picture does not come back through here.** It goes from VideoToolbox's callback
//! directly into the channel the sender reads (see [`Encoder::sending_to`]). Draining it on the capture
//! callback would mean the last picture of a burst waits for the *next* frame to arrive — and
//! ScreenCaptureKit only delivers a frame when the screen changes, so on a still screen that wait is
//! unbounded.
//!
//! What is left is the awkward part of any delegate in Rust: an Objective-C object whose method is
//! called by the framework, holding Rust state. [`Output`] is that object, its state is in its ivars,
//! and the [`Mutex`] around the encoder is what makes the arrangement sound rather than hopeful — the
//! queue is serial, so it is never actually contended.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _, Result};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput,
    SCStreamOutputType,
};

use super::content::{self, Located, Target};
use super::encode::{Encoder, Picture};

/// How many pictures may wait for the sender.
///
/// The same reasoning as the encoder's own queue: enough to absorb a hiccup, small enough that a stall
/// drops frames rather than showing old ones.
const QUEUE: usize = 8;

/// How many captured frames ScreenCaptureKit may hold for us.
///
/// Five. The framework's own advice is between three and eight; a deeper queue only means the
/// framework holding pixel buffers we are already too late to encode.
const DEPTH: isize = 5;

/// A capture in progress. Dropping it stops the stream and lets go of the encoder.
pub struct Capture {
    stream: Retained<SCStream>,
    /// The delegate. Held because the framework does *not* retain it — an output object that goes out of
    /// scope here is a stream that stops delivering, silently.
    output: Retained<Output>,
    /// Where finished pictures arrive. Taken out by [`Capture::pictures`] so the sending thread can
    /// wait on it rather than poll — after which [`Capture::take`] has nothing to give.
    pictures: Option<Receiver<Picture>>,
    frames: Arc<AtomicU64>,
    trouble: Arc<Mutex<Option<String>>>,
    /// The size the stream was configured at, which is what the far side is told to expect.
    pub width: u32,
    pub height: u32,
}

// SAFETY: both are ordinary Objective-C objects, which may be retained and released from any thread.
// The Rust state inside the delegate is reached only through the `Mutex` in its ivars, and the
// framework calls the delegate on one serial queue.
unsafe impl Send for Capture {}

impl Capture {
    /// Start capturing `target`, scaled to fit within `width`×`height`.
    pub fn start(target: Target, width: u32, height: u32, fps: u32, kbps: u32) -> Result<Capture> {
        let located = content::locate(target).map_err(|why| anyhow!("{why}"))?;
        let (native_width, native_height) = located.pixels();
        let (width, height) = fit(native_width, native_height, width, height);

        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let encoder = Encoder::sending_to(width as i32, height as i32, fps, kbps, tx)?;

        let frames = Arc::new(AtomicU64::new(0));
        let trouble = Arc::new(Mutex::new(None));
        let output = Output::new(encoder, frames.clone(), trouble.clone());

        let filter = filter(&located);
        let configuration = configure(width, height, fps);

        // SAFETY: the filter and configuration are ours and fully initialised; the delegate outlives
        // the stream because `Capture` holds both and drops the stream first.
        let stream = unsafe {
            let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(delegate),
            )
        };

        // A serial queue of our own rather than the framework's: it is the thing that makes access to
        // the encoder one-at-a-time, and it keeps the name recognisable in a sample or a crash report.
        let queue = DispatchQueue::new("dev.boavoice.screen", DispatchQueueAttr::SERIAL);

        // SAFETY: the output object conforms to the protocol (see `define_class!` below) and the queue
        // is a live serial queue.
        unsafe {
            let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    sink,
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|error| anyhow!("{}", error.localizedDescription()))
                .context("attaching the screen output")?;
        }

        start(&stream)?;
        log::info!(
            "screen: capturing {} at {width}×{height} ({native_width}×{native_height} native), \
             {fps} fps, {kbps} kbit/s",
            located.label()
        );
        crate::diagnostics::note(&format!(
            "screen: ScreenCaptureKit {width}×{height} at {fps} fps"
        ));

        Ok(Capture { stream, output, pictures: Some(rx), frames, trouble, width, height })
    }

    /// The next encoded picture, if one is ready and nobody has taken the channel over.
    pub fn take(&self) -> Option<Picture> {
        self.pictures.as_ref()?.try_recv().ok()
    }

    /// Take the picture channel, for a thread that would rather block than poll.
    ///
    /// Once. A second caller gets `None` — two consumers of one stream would each see half the frames,
    /// which is the sort of bug that looks like a slow network.
    pub fn pictures(&mut self) -> Option<Receiver<Picture>> {
        self.pictures.take()
    }

    /// How many frames the window server has delivered.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Why the stream stopped, if it did.
    ///
    /// The framework's own way of saying "the window you were sharing has closed" or "the permission was
    /// taken away", and both are things the user should be told rather than left looking at a frozen
    /// picture.
    pub fn trouble(&self) -> Option<String> {
        self.trouble.lock().ok().and_then(|trouble| trouble.clone())
    }

    /// Ask for a keyframe on the next frame, so somebody who has just joined can start decoding.
    pub fn want_keyframe(&self) {
        self.output.ivars().keyframe.store(true, Ordering::Release);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Synchronous on purpose. The screen-recording indicator in the menu bar stays lit until the
        // stream is actually stopped, and an asynchronous stop would leave it lit for as long as it took
        // — which reads as "this app is still watching you".
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let handler = block2::RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.try_send(());
        });
        // SAFETY: the handler is called once; the block lives until then.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        crate::diagnostics::note("screen: capture stopped");
    }
}

/// Start the stream, waiting for the framework to say whether it worked.
///
/// Blocking here is the point: a failure arrives in the completion handler rather than from the call, so
/// a version that returned immediately would report success and then deliver nothing.
fn start(stream: &SCStream) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);
    let handler = block2::RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the framework passes either null or a live error, borrowed for the call.
        let why = unsafe { error.as_ref() }.map(|error| {
            let code = error.code();
            match code {
                -3801 => "screen recording was refused. System Settings → Privacy & Security → \
                          Screen & System Audio Recording → BoaVoice, then restart the app."
                    .to_string(),
                _ => format!("{} (code {code})", error.localizedDescription()),
            }
        });
        let _ = tx.try_send(why);
    });
    // SAFETY: as above.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(None) => Ok(()),
        Ok(Some(why)) => Err(anyhow!("{why}")),
        Err(_) => Err(anyhow!("ScreenCaptureKit did not start within five seconds")),
    }
}

/// The content filter for one target.
fn filter(located: &Located) -> Retained<SCContentFilter> {
    // SAFETY: both initialisers take borrowed framework objects and return an initialised filter.
    unsafe {
        match located {
            // Excluding nothing, which is the filter that means "this whole screen". The alternative,
            // `initWithDisplay:includingWindows:`, would leave out the desktop and the dock.
            Located::Display(display) => SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                display,
                &NSArray::new(),
            ),
            // "Desktop independent" means the window and its shadow, wherever it is and whatever is in
            // front of it — which is what somebody sharing one window expects, rather than a rectangle
            // of the screen that other windows can cover.
            Located::Window(window) => SCContentFilter::initWithDesktopIndependentWindow(
                SCContentFilter::alloc(),
                window,
            ),
        }
    }
}

/// The stream configuration.
fn configure(width: u32, height: u32, fps: u32) -> Retained<SCStreamConfiguration> {
    // SAFETY: a plain allocation, and then property setters on the object it returns.
    let configuration = unsafe { SCStreamConfiguration::new() };
    unsafe {
        configuration.setWidth(width as usize);
        configuration.setHeight(height as usize);
        // The encoder's own format. Asking the window server for BGRA and converting would be a
        // full-frame colour conversion per frame, on the CPU, for nothing: VideoToolbox takes this
        // directly.
        configuration.setPixelFormat(u32::from_be_bytes(*b"420v"));
        // A ceiling on the frame rate, not a floor: the framework delivers a frame when the screen
        // changes, so a still screen costs nothing.
        configuration.setMinimumFrameInterval(CMTime {
            value: 1,
            timescale: fps.max(1) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        });
        configuration.setQueueDepth(DEPTH);
        // The pointer is part of what somebody is showing you. Without it, following along with what
        // the other person is doing is guesswork.
        configuration.setShowsCursor(true);
        // Scaled to fit rather than cropped, and the aspect ratio kept: a share that quietly cuts the
        // edges off is worse than one that is smaller than asked for.
        configuration.setScalesToFit(true);
        configuration.setPreservesAspectRatio(true);
    }
    configuration
}

/// The size to capture at: the source's own, scaled down to fit the cap, and even.
///
/// Even because H.264 chroma is subsampled by two in both directions, and an odd dimension is an
/// encoder either refusing the size or quietly rounding it — which changes the aspect ratio.
fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let max_width = max_width.clamp(2, crate::screen::MAX_DIMENSION);
    let max_height = max_height.clamp(2, crate::screen::MAX_DIMENSION);
    let (width, height) = (width.max(2), height.max(2));

    let scale = (max_width as f64 / width as f64).min(max_height as f64 / height as f64).min(1.0);
    let even = |value: f64| ((value.round() as u32).max(2) / 2) * 2;
    (even(width as f64 * scale), even(height as f64 * scale))
}

// --------------------------------------------------------------------------- //
// The delegate
// --------------------------------------------------------------------------- //

/// What the delegate holds on to.
pub struct Ivars {
    /// The encoder, behind a lock because the framework owns the thread it is used from. The queue is
    /// serial, so this is never contended in practice — it is here to make the sharing legal rather
    /// than to arbitrate it.
    encoder: Mutex<Encoder>,
    frames: Arc<AtomicU64>,
    trouble: Arc<Mutex<Option<String>>>,
    /// Set when the next frame should be a keyframe, cleared when it has been asked for.
    keyframe: std::sync::atomic::AtomicBool,
}

define_class!(
    // SAFETY: `NSObject` is a valid superclass and the class has no `dealloc` requirements beyond
    // dropping its ivars, which the macro arranges.
    #[unsafe(super(NSObject))]
    #[name = "BoaVoiceStreamOutput"]
    #[ivars = Ivars]
    pub struct Output;

    impl Output {}

    unsafe impl NSObjectProtocol for Output {}

    /// The frames.
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // SAFETY: the sample buffer is borrowed for the duration of this call, which is all the
            // encoder needs — it retains the image itself if it has to.
            let Some(image) = (unsafe { sample.image_buffer() }) else {
                // A sample with no image is how the framework says "nothing changed, but here is a
                // heartbeat". Normal, and nothing to encode.
                return;
            };

            let ivars = self.ivars();
            let force = ivars.keyframe.swap(false, Ordering::AcqRel);
            let Ok(mut encoder) = ivars.encoder.lock() else { return };
            match encoder.encode(&image, force) {
                Ok(()) => {
                    ivars.frames.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => log::debug!("screen: {err:#}"),
            }
        }
    }

    /// The bad news.
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            let why = format!("{} (code {})", error.localizedDescription(), error.code());
            log::warn!("screen: the capture stopped: {why}");
            crate::diagnostics::note(&format!("screen: capture stopped: {why}"));
            if let Ok(mut trouble) = self.ivars().trouble.lock() {
                *trouble = Some(why);
            }
        }
    }
);

impl Output {
    fn new(
        encoder: Encoder,
        frames: Arc<AtomicU64>,
        trouble: Arc<Mutex<Option<String>>>,
    ) -> Retained<Output> {
        let this = Output::alloc().set_ivars(Ivars {
            encoder: Mutex::new(encoder),
            frames,
            trouble,
            // The first frame: a stream whose first picture is not a keyframe is a stream nobody can
            // start watching.
            keyframe: std::sync::atomic::AtomicBool::new(true),
        });
        // SAFETY: `NSObject`'s `init` on freshly allocated storage whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap is a ceiling, and the aspect ratio survives it. This is the function that decides what
    /// the far side receives, and getting it wrong is a stretched picture rather than an error.
    #[test]
    fn a_capture_fits_inside_the_cap_without_stretching() {
        // A 4K screen capped at 1080p: halved, both dimensions, aspect kept.
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        // Wider than the cap allows: limited by width, and the height follows.
        assert_eq!(fit(3000, 1000, 1500, 1000), (1500, 500));
        // Smaller than the cap is left alone rather than upscaled — there is nothing to gain from
        // sending more pixels than the source has.
        assert_eq!(fit(800, 600, 1920, 1080), (800, 600));
        // Odd numbers do not survive: H.264 wants even ones.
        assert_eq!(fit(999, 501, 4000, 4000), (998, 500));
        // Nothing produces a zero dimension, whatever it is asked for.
        let (width, height) = fit(1, 1, 0, 0);
        assert!(width >= 2 && height >= 2, "{width}×{height}");
        // The decoder ceiling holds even when the settings ask for more.
        let (width, _) = fit(8000, 4000, 8000, 4000);
        assert!(width <= crate::screen::MAX_DIMENSION, "{width}");
    }

    /// The real thing, end to end: the main screen, captured, encoded, and a keyframe out the other
    /// side. Skipped rather than failed where screen recording is not granted, because that is a
    /// property of the machine and not of the code.
    #[test]
    fn the_main_screen_produces_a_keyframe() {
        let Ok(sources) = content::sources() else {
            eprintln!("no shareable content here — skipping");
            return;
        };
        let Some(screen) = sources.iter().find(|source| !source.window) else {
            eprintln!("no screen to capture — skipping");
            return;
        };
        let target = content::target(screen).expect("a screen source is addressable");

        let capture = match Capture::start(target, 1280, 720, 30, 2_000) {
            Ok(capture) => capture,
            Err(err) => {
                eprintln!("no capture here: {err:#}");
                return;
            }
        };
        assert!(capture.width <= 1280 && capture.height <= 720);

        // Up to three seconds. The framework delivers a frame when the screen changes, and a test
        // machine with nothing moving on it can take a moment to produce the first one.
        let mut keyframe = None;
        for _ in 0..300 {
            if let Some(picture) = capture.take() {
                if picture.keyframe {
                    keyframe = Some(picture);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(capture.trouble().is_none(), "{:?}", capture.trouble());

        let keyframe = keyframe.expect("a keyframe within three seconds");
        assert_eq!(&keyframe.data[..4], &[0, 0, 0, 1], "Annex-B starts with a start code");
        assert_eq!(keyframe.data[4] & 0x1F, 7, "a keyframe leads with its SPS");
        assert!(keyframe.data.len() > 500, "suspiciously small: {}", keyframe.data.len());
        assert!(capture.frames() > 0, "the window server delivered nothing");
    }
}
