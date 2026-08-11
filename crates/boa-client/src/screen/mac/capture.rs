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

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Context as _, Result};
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::CVImageBuffer;
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
    /// scope here is a stream that stops delivering, silently. Never read, and that is the point: it
    /// exists to stay alive.
    #[allow(dead_code)]
    output: Retained<Output>,
    /// Where finished pictures arrive. Taken out by [`Capture::pictures`] so the sending thread can
    /// wait on it rather than poll — after which [`Capture::take`] has nothing to give.
    pictures: Option<Receiver<Picture>>,
    /// The machine's own sound, as interleaved stereo at 48 kHz, when it was asked for.
    sound: Option<Receiver<Vec<f32>>>,
    frames: Arc<AtomicU64>,
    trouble: Arc<Mutex<Option<String>>>,
    /// The shared encoder state, kept so a keyframe can be asked for and the heartbeat stopped.
    beat: Arc<Heartbeat>,
    heart: Option<std::thread::JoinHandle<()>>,
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
    ///
    /// With `sound`, the stream also carries the machine's own output — every process's audio except
    /// this one's. That exclusion is the whole reason this is worth doing rather than reading a loopback
    /// device: a loopback hears the call it is in, so everybody in it hears themselves back.
    pub fn start(
        target: Target,
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
        sound: bool,
    ) -> Result<Capture> {
        let located = content::locate(target).map_err(|why| anyhow!("{why}"))?;
        let (native_width, native_height) = located.pixels();
        let (width, height) = fit(native_width, native_height, width, height);

        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let encoder = Encoder::sending_to(width as i32, height as i32, fps, kbps, tx)?;

        let frames = Arc::new(AtomicU64::new(0));
        let trouble = Arc::new(Mutex::new(None));
        let beat = Arc::new(Heartbeat {
            encoder: Mutex::new(encoder),
            last: Mutex::new(None),
            fed: Mutex::new(Instant::now()),
            began: Instant::now(),
            frames: frames.clone(),
            // The first frame: a stream whose first picture is not a keyframe is a stream nobody can
            // start watching.
            keyframe: std::sync::atomic::AtomicBool::new(true),
            stop: std::sync::atomic::AtomicBool::new(false),
        });

        // The sound, when it was asked for: a channel of interleaved stereo, which is what the Opus
        // encoder on the other end of it wants. A shallow queue — a share whose sound is a second
        // behind is worse than one that dropped a moment of it.
        let (sound_tx, sound_rx) = if sound {
            let (tx, rx) = std::sync::mpsc::sync_channel(16);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let output = Output::new(beat.clone(), trouble.clone(), sound_tx);

        let filter = filter(&located);
        let configuration = configure(width, height, fps, sound);

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

        // The sound is a second output on a queue of its own. Sharing the video queue would put the
        // Opus path behind whatever the encoder is doing with a 4K frame, and audio is the one of the
        // two that nobody forgives being late.
        if sound {
            let queue = DispatchQueue::new("dev.boavoice.screen.audio", DispatchQueueAttr::SERIAL);
            // SAFETY: as above.
            let attached = unsafe {
                let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
                stream.addStreamOutput_type_sampleHandlerQueue_error(
                    sink,
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
            };
            // Not fatal. A share with a picture and no sound is worth having; refusing to share at all
            // because the sound could not be attached is not.
            if let Err(error) = attached {
                log::warn!("screen: no desktop audio: {}", error.localizedDescription());
                crate::diagnostics::note("screen: the sound output was refused");
            }
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

        // The heartbeat, started only once the stream is running so it does not send a frame into a
        // capture that failed.
        let heart = {
            let beat = beat.clone();
            std::thread::Builder::new()
                .name("boa-screen-heartbeat".into())
                .spawn(move || keep_alive(&beat))
                .context("spawning the screen heartbeat")?
        };

        Ok(Capture {
            stream,
            output,
            pictures: Some(rx),
            sound: sound_rx,
            frames,
            trouble,
            beat,
            heart: Some(heart),
            width,
            height,
        })
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

    /// Take the sound channel, if this capture has one. Once, like [`Capture::pictures`].
    pub fn sound(&mut self) -> Option<Receiver<Vec<f32>>> {
        self.sound.take()
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

    /// Change the bitrate the encoder is aiming at, while it runs.
    pub fn set_bitrate(&self, kbps: u32) {
        if let Ok(encoder) = self.beat.encoder.lock() {
            encoder.set_bitrate(kbps);
        }
    }

    /// Ask for a keyframe on the next frame, so somebody who has just joined can start decoding.
    pub fn want_keyframe(&self) {
        self.beat.keyframe.store(true, Ordering::Release);
    }
}

/// Re-send the last frame while nothing is changing.
///
/// The encoder's own rules then apply to it: a repeat encodes to a few hundred bytes, and its
/// "keyframe at least every two seconds" limit — measured in the wall-clock timestamps
/// [`Encoder::encode`] is given — means a still share becomes watchable within two seconds of somebody
/// joining, instead of never.
fn keep_alive(beat: &Heartbeat) {
    while !beat.stop.load(Ordering::Acquire) {
        std::thread::sleep(HEARTBEAT / 4);
        if beat.stop.load(Ordering::Acquire) {
            return;
        }
        let idle = beat.fed.lock().map(|fed| fed.elapsed()).unwrap_or_default();
        if idle < HEARTBEAT {
            continue;
        }
        // Cloned out of the lock before encoding: `feed` takes the encoder's lock, and holding two is
        // how a deadlock is written.
        let last = beat.last.lock().ok().and_then(|last| last.as_ref().map(|i| i.0.clone()));
        let Some(image) = last else { continue };
        beat.feed(&image, false);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.beat.stop.store(true, Ordering::Release);
        if let Some(heart) = self.heart.take() {
            let _ = heart.join();
        }
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
fn configure(width: u32, height: u32, fps: u32, sound: bool) -> Retained<SCStreamConfiguration> {
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

        if sound {
            configuration.setCapturesAudio(true);
            // **Without this the call feeds back.** The share would carry the voices coming out of this
            // app's own speakers, so everybody in the call would hear themselves a moment late. This
            // one line is what a loopback device cannot do.
            configuration.setExcludesCurrentProcessAudio(true);
            // The rate and channel count the Opus encoder downstream is configured for. Asking here
            // means the framework resamples rather than this code.
            configuration.setSampleRate(boa_proto::media::VOICE_SAMPLE_RATE as isize);
            configuration.setChannelCount(2);
        }
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

/// How long a still screen may go without producing anything.
///
/// **The reason this exists at all:** ScreenCaptureKit delivers a frame when the screen *changes*, and
/// nothing whatsoever when it does not. A share of a window that is sitting still therefore sends no
/// packets — so somebody who starts watching it sees "waiting for a keyframe" and keeps seeing it, for
/// as long as nobody moves anything. That is not a rare case; it is a slide, a document, a paused video.
///
/// Half a second. The re-sent frame is identical to the last one, which H.264 encodes to a few hundred
/// bytes, and it lets the encoder's own two-second keyframe rule do the rest.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(500);

/// The encoder and the last thing given to it, shared between the framework's queue and the heartbeat.
///
/// One `Arc` rather than state in the delegate's ivars, because the heartbeat runs on a thread of its
/// own and an Objective-C object is not the thing to send there.
struct Heartbeat {
    /// The encoder, behind a lock because two threads feed it: ScreenCaptureKit's queue and the
    /// heartbeat. Contention is a few microseconds twice a second.
    encoder: Mutex<Encoder>,
    /// The last frame the window server delivered, kept so it can be sent again. This is the only
    /// reason a still share keeps working.
    last: Mutex<Option<Image>>,
    /// When something was last handed to the encoder — by either thread.
    fed: Mutex<Instant>,
    /// When the capture began, which is what encoder timestamps are measured from.
    began: Instant,
    frames: Arc<AtomicU64>,
    /// Set when the next frame should be a keyframe, cleared when it has been asked for.
    keyframe: std::sync::atomic::AtomicBool,
    stop: std::sync::atomic::AtomicBool,
}

/// A captured frame that can be held across threads.
///
/// `CVImageBuffer` is a Core Video object: retaining, releasing and reading one from another thread is
/// allowed, and reading is all that happens here — the encoder copies what it needs. The wrapper exists
/// because that guarantee is Apple's documentation rather than something the bindings express.
struct Image(CFRetained<CVImageBuffer>);

// SAFETY: see [`Image`] — the buffer is read-only from here on, and access is serialised by the `Mutex`
// it lives in.
unsafe impl Send for Image {}

impl Heartbeat {
    /// Hand a frame to the encoder and note the time.
    fn feed(&self, image: &CVImageBuffer, force_keyframe: bool) {
        let at = self.began.elapsed();
        let Ok(mut encoder) = self.encoder.lock() else { return };
        match encoder.encode(image, force_keyframe, at) {
            Ok(()) => {
                if let Ok(mut fed) = self.fed.lock() {
                    *fed = Instant::now();
                }
            }
            Err(err) => log::debug!("screen: {err:#}"),
        }
    }
}

/// What the delegate holds on to.
pub struct Ivars {
    beat: Arc<Heartbeat>,
    trouble: Arc<Mutex<Option<String>>>,
    /// Where the machine's sound goes. Reached from the audio queue, which is a different thread from
    /// the video one — hence a channel rather than shared state.
    sound: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
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
            if kind == SCStreamOutputType::Audio {
                self.did_hear(sample);
                return;
            }
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

            let beat = &self.ivars().beat;
            let force = beat.keyframe.swap(false, Ordering::AcqRel);
            beat.frames.fetch_add(1, Ordering::Relaxed);
            beat.feed(&image, force);
            // Kept for the heartbeat to send again if the screen goes quiet. Stored after encoding, so a
            // slow encoder never delays the frame it was given.
            if let Ok(mut last) = beat.last.lock() {
                *last = Some(Image(image));
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

/// An `AudioBufferList` with room for two buffers.
///
/// The framework's own type has room for exactly one, because the real thing is variable-length and C
/// expresses that by lying about the array size. Stereo arrives as *two* buffers — one per channel —
/// so a call handed the declared type would have the second buffer written past the end of it. This is
/// the same layout with the array it actually needs.
#[repr(C)]
struct TwoBuffers {
    count: u32,
    buffers: [objc2_core_audio_types::AudioBuffer; 2],
}

impl Output {
    /// One buffer of the machine's own sound.
    ///
    /// ScreenCaptureKit delivers **deinterleaved** floats: one buffer per channel, each a run of samples
    /// for that channel alone. Opus wants them interleaved, left and right alternating, so that is what
    /// this does — and it is the whole job, because the sample rate and channel count were asked for in
    /// the configuration.
    fn did_hear(&self, sample: &CMSampleBuffer) {
        let Some(sound) = self.ivars().sound.as_ref() else { return };

        let mut list = TwoBuffers {
            count: 2,
            buffers: [objc2_core_audio_types::AudioBuffer {
                mNumberChannels: 0,
                mDataByteSize: 0,
                mData: std::ptr::null_mut(),
            }; 2],
        };
        let mut block: *mut objc2_core_media::CMBlockBuffer = std::ptr::null_mut();

        // SAFETY: the list is the right size for the two buffers it declares, and the block buffer this
        // retains is released below — the "retained" in the name is the caller's job, and leaking it
        // would leak an audio buffer twenty times a second.
        let status = unsafe {
            sample.audio_buffer_list_with_retained_block_buffer(
                std::ptr::null_mut(),
                &mut list as *mut TwoBuffers as *mut objc2_core_audio_types::AudioBufferList,
                std::mem::size_of::<TwoBuffers>(),
                None,
                None,
                0,
                &mut block,
            )
        };
        // SAFETY: on success this is a retained block buffer; taking it into `CFRetained` releases it
        // when this function returns.
        let block = unsafe { NonNull::new(block).map(|block| CFRetained::from_raw(block)) };
        if status != 0 {
            log::debug!("screen: could not read a sound buffer (status {status})");
            return;
        }

        let buffers = &list.buffers[..(list.count as usize).min(2)];
        let samples: Vec<&[f32]> = buffers
            .iter()
            .filter(|buffer| !buffer.mData.is_null())
            // SAFETY: the framework reports how many bytes are behind each pointer, and they are floats
            // because the configuration asked for a float format. The slices live as long as `block`.
            .map(|buffer| unsafe {
                std::slice::from_raw_parts(
                    buffer.mData as *const f32,
                    buffer.mDataByteSize as usize / 4,
                )
            })
            .collect();

        let interleaved = match samples.as_slice() {
            // Stereo, deinterleaved: the usual case.
            [left, right] => {
                let frames = left.len().min(right.len());
                let mut out = Vec::with_capacity(frames * 2);
                for index in 0..frames {
                    out.push(left[index]);
                    out.push(right[index]);
                }
                out
            }
            // One buffer. Either mono, which is doubled so it comes out of both speakers, or already
            // interleaved stereo, which is passed through — the channel count says which.
            [only] => {
                if buffers[0].mNumberChannels >= 2 {
                    only.to_vec()
                } else {
                    only.iter().flat_map(|sample| [*sample, *sample]).collect()
                }
            }
            _ => return,
        };
        drop(block);

        if interleaved.is_empty() {
            return;
        }
        // Dropped rather than waited for. This is a framework callback: blocking it stalls the audio
        // path inside the window server's client, and a moment of missing sound beats that.
        if sound.try_send(interleaved).is_err() {
            log::trace!("screen: dropped a sound buffer");
        }
    }

    fn new(
        beat: Arc<Heartbeat>,
        trouble: Arc<Mutex<Option<String>>>,
        sound: Option<std::sync::mpsc::SyncSender<Vec<f32>>>,
    ) -> Retained<Output> {
        let this = Output::alloc().set_ivars(Ivars { beat, trouble, sound });
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

    /// **The interop check that matters.** VideoToolbox encodes High profile; the watchers decode with
    /// openh264, in-process. If those two do not agree, every share in this app is a black rectangle on
    /// everybody else's machine — and nothing else in the test suite would notice, because both halves
    /// work perfectly on their own.
    ///
    /// So: capture the real screen, encode with the real hardware, and decode with the real decoder the
    /// receiving side uses.
    #[test]
    fn what_this_encodes_is_what_the_watchers_can_decode() {
        let Ok(sources) = content::sources() else { return };
        let Some(screen) = sources.iter().find(|source| !source.window) else { return };
        let target = content::target(screen).expect("a screen source is addressable");

        let capture = match Capture::start(target, 960, 540, 30, 1_500, false) {
            Ok(capture) => capture,
            Err(err) => {
                eprintln!("no capture here: {err:#}");
                return;
            }
        };

        let mut decoder = match openh264::decoder::Decoder::new() {
            Ok(decoder) => decoder,
            Err(err) => {
                eprintln!("no decoder here: {err}");
                return;
            }
        };

        // A keyframe first — a delta picture decoded without its reference is noise — then a few more,
        // because a decoder that manages the keyframe and chokes on the deltas is the subtler failure.
        let mut seen_keyframe = false;
        let mut decoded = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while decoded < 4 && std::time::Instant::now() < deadline {
            let Some(picture) = capture.take() else {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            };
            seen_keyframe |= picture.keyframe;
            if !seen_keyframe {
                continue;
            }
            match decoder.decode(&picture.data) {
                // A picture that produced nothing is normal — the decoder holds one back.
                Ok(None) => {}
                Ok(Some(frame)) => {
                    use openh264::formats::YUVSource as _;
                    let (width, height) = frame.dimensions();
                    assert_eq!(
                        (width as u32, height as u32),
                        (capture.width, capture.height),
                        "the decoder disagrees with the encoder about the size"
                    );
                    decoded += 1;
                }
                Err(err) => panic!(
                    "openh264 could not decode what VideoToolbox produced: {err} \
                     (keyframe: {}, {} bytes)",
                    picture.keyframe,
                    picture.data.len()
                ),
            }
        }

        assert!(seen_keyframe, "no keyframe within five seconds");
        assert!(decoded >= 1, "nothing decoded — a share would be a black rectangle");
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

        let mut capture = match Capture::start(target, 1280, 720, 30, 2_000, true) {
            Ok(capture) => capture,
            Err(err) => {
                eprintln!("no capture here: {err:#}");
                return;
            }
        };
        assert!(capture.width <= 1280 && capture.height <= 720);
        // Asked for sound, so there has to be a channel for it — whether anything is playing on this
        // machine is not something a test can arrange.
        let sound = capture.sound().expect("a capture asked for sound has a sound channel");

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

        // Whatever sound did arrive has to be interleaved stereo: an odd number of samples means the
        // channels have been read wrongly, and that is a stream that plays as noise.
        while let Ok(samples) = sound.try_recv() {
            assert_eq!(samples.len() % 2, 0, "not interleaved stereo: {} samples", samples.len());
            assert!(samples.iter().all(|sample| sample.is_finite()), "sound with no numbers in it");
        }

        let keyframe = keyframe.expect("a keyframe within three seconds");
        assert_eq!(&keyframe.data[..4], &[0, 0, 0, 1], "Annex-B starts with a start code");
        assert_eq!(keyframe.data[4] & 0x1F, 7, "a keyframe leads with its SPS");
        assert!(keyframe.data.len() > 500, "suspiciously small: {}", keyframe.data.len());
        assert!(capture.frames() > 0, "the window server delivered nothing");
    }
}
