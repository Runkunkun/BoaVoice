//! H.264 with the hardware encoder, through VideoToolbox.
//!
//! This is what ffmpeg was doing, done in-process. It matters for three reasons and only the third is
//! about speed: it removes a 76 MB GPL binary from the app, it removes a subprocess whose stdout has to
//! be parsed for NAL units, and the encoder it reaches is the same silicon block ffmpeg's
//! `h264_videotoolbox` was reaching anyway.
//!
//! Two pieces of it are genuinely fiddly, and both are where a naive version produces a stream that
//! decodes to nothing.
//!
//! **The output is AVCC, not Annex-B.** VideoToolbox hands back NAL units prefixed with a 4-byte
//! big-endian length. The wire format here — and what every decoder expects to be fed — separates them
//! with start codes instead. Converting is a matter of replacing each length with `00 00 00 01`, and
//! forgetting to leaves a decoder with what looks like a corrupt first NAL.
//!
//! **The parameter sets are not in the stream.** SPS and PPS live in the sample buffer's *format
//! description*, not in its data. A decoder handed only the slices has nothing to decode against, so
//! they are read out of the format description and put in front of every keyframe. That also makes each
//! keyframe independently decodable, which is what lets somebody join a share mid-stream.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, SyncSender};

use anyhow::{anyhow, bail, Result};
use objc2_core_foundation::{
    kCFBooleanFalse, kCFBooleanTrue, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString,
    CFType,
};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoCodecType, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, VTCompressionSession, VTEncodeInfoFlags,
};

/// One encoded picture, in the form the wire wants.
pub struct Picture {
    /// Annex-B: start code, NAL, start code, NAL…
    pub data: Vec<u8>,
    /// Independently decodable, with its parameter sets in front.
    pub keyframe: bool,
}

/// How many encoded pictures may wait to be collected.
///
/// Small on purpose. The encoder's callback runs on VideoToolbox's own thread and must not block, and a
/// deep queue would mean sending pictures that are already stale after a stall. Six is a tenth of a
/// second at sixty frames.
const QUEUE: usize = 6;

/// What the C callback writes into. Reached through a raw pointer, so it is boxed and outlives the
/// session by construction — see [`Encoder::drop`].
struct Sink {
    pictures: SyncSender<Picture>,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    /// The boxed [`Sink`] the callback holds a pointer to. Freed after the session is invalidated,
    /// never before: an in-flight callback would otherwise write into freed memory.
    sink: *mut Sink,
    pictures: Receiver<Picture>,
    /// Frame index, which becomes the presentation timestamp. VideoToolbox needs timestamps to be
    /// increasing and otherwise does not care what they mean.
    frame: i64,
    fps: u32,
}

impl Encoder {
    /// Start an encoder for a picture of this size.
    pub fn new(width: i32, height: i32, fps: u32, kbps: u32) -> Result<Encoder> {
        if width <= 0 || height <= 0 {
            bail!("a {width}×{height} encoder is not a thing");
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let sink = Box::into_raw(Box::new(Sink { pictures: tx }));

        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        // `avc1` — H.264. The specification dictionary asks for hardware, which on every Mac this runs
        // on is a dedicated block rather than the CPU.
        let hardware = dictionary(&[(
            unsafe { objc2_video_toolbox::kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder },
            boolean(true),
        )])?;

        // SAFETY: the out-pointer is valid, the callback matches the expected signature, and the refcon
        // is the boxed sink which lives until this session is invalidated.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
                u32::from_be_bytes(*b"avc1") as CMVideoCodecType,
                Some(&hardware),
                None,
                None,
                Some(on_encoded),
                sink as *mut c_void,
                NonNull::new(&mut out).expect("a stack slot is never null"),
            )
        };
        if status != 0 || out.is_null() {
            // SAFETY: nothing took ownership of the sink, so it is ours to drop.
            unsafe { drop(Box::from_raw(sink)) };
            bail!("VideoToolbox refused a {width}×{height} encoder (status {status})");
        }
        // SAFETY: `create` returned a retained session.
        let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) };

        let encoder = Encoder { session, sink, pictures: rx, frame: 0, fps: fps.max(1) };
        encoder.configure(kbps)?;
        log::info!("screen: VideoToolbox encoder {width}×{height} at {fps} fps, {kbps} kbit/s");
        Ok(encoder)
    }

    /// The properties that make this a *live* encoder rather than a file encoder.
    fn configure(&self, kbps: u32) -> Result<()> {
        let fps = self.fps as i32;
        // SAFETY: every key is a framework constant and every value's type matches what the key
        // documents. A rejected property is logged rather than fatal: an encoder that ignored one is
        // still an encoder, and refusing to share because a hint was declined would be worse.
        unsafe {
            // Real time: encode as frames arrive rather than buffering to optimise. This is the single
            // most important one — without it the encoder introduces most of a second of latency.
            self.set(kVTCompressionPropertyKey_RealTime, boolean(true));
            // No frame reordering, which means no B-frames. A B-frame cannot be decoded until the
            // picture *after* it has arrived, so for a live screen it buys a few percent of bitrate at
            // the cost of showing everything one frame late.
            self.set(kVTCompressionPropertyKey_AllowFrameReordering, boolean(false));
            self.set(kVTCompressionPropertyKey_ProfileLevel, string(kVTProfileLevel_H264_High_AutoLevel));
            self.set(kVTCompressionPropertyKey_AverageBitRate, number_i32((kbps * 1000) as i32));
            self.set(kVTCompressionPropertyKey_ExpectedFrameRate, number_i32(fps));
            // A keyframe every two seconds, expressed both ways: the frame count is what the encoder
            // enforces, the duration is what it falls back to when the frame rate turns out lower than
            // expected — which on a static screen it will.
            self.set(kVTCompressionPropertyKey_MaxKeyFrameInterval, number_i32(fps * 2));
            self.set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, number_f64(2.0));
        }
        Ok(())
    }

    /// Set one property, logging a refusal rather than failing.
    ///
    /// # Safety
    /// `key` must be a VideoToolbox property key and `value` of the type it documents.
    unsafe fn set(&self, key: &CFString, value: Option<CFRetained<CFType>>) {
        let Some(value) = value else { return };
        // A free function rather than a method: the property API is on `VTSession`, which a
        // compression session *is* without the bindings expressing it.
        let status = unsafe {
            objc2_video_toolbox::VTSessionSetProperty(
                &self.session as &objc2_video_toolbox::VTSession,
                key,
                Some(&value),
            )
        };
        if status != 0 {
            log::debug!("screen: encoder declined {key} (status {status})");
        }
    }

    /// Hand one captured frame to the encoder.
    ///
    /// Returns as soon as the frame is queued; the encoded picture arrives later, on the encoder's own
    /// thread, and is collected with [`Encoder::take`].
    pub fn encode(&mut self, image: &CVImageBuffer, force_keyframe: bool) -> Result<()> {
        // A timescale of the frame rate and a timestamp of the frame number: the encoder only needs
        // these to increase, and deriving them from the frame count rather than from the clock means a
        // late frame does not look like a rate change.
        let timestamp = CMTime {
            value: self.frame,
            timescale: self.fps as i32,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        };
        let duration = CMTime {
            value: 1,
            timescale: self.fps as i32,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        };
        self.frame += 1;

        let properties = if force_keyframe {
            // SAFETY: the key is a framework constant and takes a boolean.
            dictionary(&[(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }, boolean(true))]).ok()
        } else {
            None
        };

        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: the image buffer is borrowed for the duration of the call, which is what the API
        // documents; the encoder retains it itself if it needs to.
        let status = unsafe {
            self.session.encode_frame(
                image,
                timestamp,
                duration,
                properties.as_deref(),
                std::ptr::null_mut(),
                &mut flags,
            )
        };
        if status != 0 {
            return Err(anyhow!("the encoder rejected a frame (status {status})"));
        }
        Ok(())
    }

    /// The next encoded picture, if one is ready.
    pub fn take(&self) -> Option<Picture> {
        self.pictures.try_recv().ok()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Invalidate first. It waits for in-flight callbacks, which is exactly the guarantee needed
        // before the sink they write into can be freed — freeing it first is a use-after-free on
        // VideoToolbox's thread, and one that would only show up under load.
        unsafe {
            self.session.invalidate();
            drop(Box::from_raw(self.sink));
        }
    }
}

/// VideoToolbox's callback: one encoded picture, on its own thread.
///
/// Everything expensive stays out of here — it converts and hands over. A `try_send` rather than a
/// `send`: if nobody has collected the last few pictures, the newest one is the one worth having and
/// blocking the encoder is never the right answer.
unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if status != 0 {
        log::debug!("screen: encode failed (status {status})");
        return;
    }
    // SAFETY: the refcon is the boxed sink this session was created with, alive until after
    // `invalidate` has returned.
    let Some(sink) = (unsafe { (refcon as *mut Sink).as_ref() }) else { return };
    // A null sample buffer means the frame was dropped, which is normal on a static screen.
    let Some(sample) = (unsafe { sample.as_ref() }) else { return };

    match annexb(sample) {
        Ok(picture) => {
            let _ = sink.pictures.try_send(picture);
        }
        Err(err) => log::debug!("screen: {err}"),
    }
}

/// Turn one encoded sample buffer into an Annex-B picture.
fn annexb(sample: &CMSampleBuffer) -> Result<Picture> {
    // SAFETY: reading the buffer the sample owns; it lives as long as the sample, which is borrowed
    // for this call.
    let block = unsafe { sample.data_buffer() }
        .ok_or_else(|| anyhow!("an encoded sample with no data"))?;

    // SAFETY: asking for the whole contiguous range. VideoToolbox's output is a single block, so
    // `length_at_offset` and the total length agree; the pointer is valid until the block is released,
    // and it is copied out before this returns.
    let mut length: usize = 0;
    let mut pointer: *mut std::ffi::c_char = std::ptr::null_mut();
    let status = unsafe {
        block.data_pointer(0, std::ptr::null_mut(), &mut length, &mut pointer)
    };
    if status != 0 || pointer.is_null() {
        bail!("could not read an encoded sample (status {status})");
    }
    // SAFETY: `length` bytes from `pointer`, as just reported.
    let avcc = unsafe { std::slice::from_raw_parts(pointer as *const u8, length) };

    // The slices first, so whether this is a keyframe can be read out of them, and the parameter sets
    // put in front afterwards. The other order would need the answer before the bytes exist.
    let mut slices = Vec::with_capacity(length + 64);
    let mut at = 0;
    while at + 4 <= avcc.len() {
        let size = u32::from_be_bytes([avcc[at], avcc[at + 1], avcc[at + 2], avcc[at + 3]]) as usize;
        at += 4;
        if size == 0 || at + size > avcc.len() {
            // A length that overruns the buffer means this is not AVCC with 4-byte lengths, which
            // would produce a stream that decodes to nothing. Better to drop the picture and say so.
            bail!("an encoded sample is not 4-byte-length AVCC");
        }
        slices.extend_from_slice(&[0, 0, 0, 1]);
        slices.extend_from_slice(&avcc[at..at + size]);
        at += size;
    }

    let keyframe = has_idr(&slices);
    let mut data = Vec::with_capacity(slices.len() + 64);
    if keyframe {
        // SPS and PPS live in the format description, not in the data. Without them in front of it, a
        // keyframe is not independently decodable and somebody joining a share sees nothing.
        for set in parameter_sets(sample) {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(&set);
        }
    }
    data.extend_from_slice(&slices);

    Ok(Picture { data, keyframe })
}

/// Whether a converted picture can be decoded on its own.
///
/// Read from the bitstream rather than from the sample's attachments, and that is a deliberate
/// simplification: the attachment route means walking a `CFArray` of `CFDictionary` and testing for the
/// *absence* of a key, three layers of Core Foundation to learn something the bytes already say. An IDR
/// slice is NAL type 5, and a picture containing one is a keyframe by definition.
fn has_idr(annexb: &[u8]) -> bool {
    annexb
        .windows(5)
        .any(|window| window[..4] == [0, 0, 0, 1] && window[4] & 0x1F == 5)
}

/// The SPS and PPS from a sample's format description.
fn parameter_sets(sample: &CMSampleBuffer) -> Vec<Vec<u8>> {
    // SAFETY: as above.
    let Some(format) = (unsafe { sample.format_description() }) else { return Vec::new() };
    let mut sets = Vec::new();

    // How many there are is only knowable by asking for index 0 with the count out-parameter.
    let mut count: usize = 0;
    let mut nal_size: i32 = 0;
    // SAFETY: out-parameters are stack slots; the pointer returned points into the format description
    // and is copied out immediately.
    unsafe {
        let mut pointer: *const u8 = std::ptr::null();
        let mut length: usize = 0;
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            &format,
            0,
            &mut pointer,
            &mut length,
            &mut count,
            &mut nal_size,
        );
        if status != 0 {
            return Vec::new();
        }
        if !pointer.is_null() && length > 0 {
            sets.push(std::slice::from_raw_parts(pointer, length).to_vec());
        }
        for index in 1..count {
            let mut pointer: *const u8 = std::ptr::null();
            let mut length: usize = 0;
            let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &format,
                index,
                &mut pointer,
                &mut length,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if status == 0 && !pointer.is_null() && length > 0 {
                sets.push(std::slice::from_raw_parts(pointer, length).to_vec());
            }
        }
    }
    sets
}

// --------------------------------------------------------------------------- //
// Core Foundation odds and ends
// --------------------------------------------------------------------------- //

fn boolean(value: bool) -> Option<CFRetained<CFType>> {
    // SAFETY: the two constants are framework statics.
    let boolean = unsafe { if value { kCFBooleanTrue } else { kCFBooleanFalse } }?;
    // SAFETY: a framework constant, retained so the dictionary can hold it.
    Some(unsafe { CFRetained::retain(NonNull::from(boolean)) }.into())
}

fn number_i32(value: i32) -> Option<CFRetained<CFType>> {
    // SAFETY: the pointer is to a live `i32` and the type says so.
    unsafe {
        CFNumber::new(None, CFNumberType::SInt32Type, &value as *const i32 as *const c_void)
            .map(Into::into)
    }
}

fn number_f64(value: f64) -> Option<CFRetained<CFType>> {
    // SAFETY: as above, for a double.
    unsafe {
        CFNumber::new(None, CFNumberType::Float64Type, &value as *const f64 as *const c_void)
            .map(Into::into)
    }
}

fn string(value: &CFString) -> Option<CFRetained<CFType>> {
    // SAFETY: as above — a live Core Foundation string.
    Some(unsafe { CFRetained::retain(NonNull::from(value)) }.into())
}

/// A small dictionary of framework keys.
fn dictionary(entries: &[(&CFString, Option<CFRetained<CFType>>)]) -> Result<CFRetained<CFDictionary>> {
    let mut keys: Vec<*const c_void> = Vec::new();
    let mut values: Vec<*const c_void> = Vec::new();
    // Held until the dictionary has been built: it retains what it is given, but not before.
    let mut alive: Vec<CFRetained<CFType>> = Vec::new();
    for (key, value) in entries {
        let Some(value) = value else { continue };
        keys.push(*key as *const CFString as *const c_void);
        values.push(CFRetained::as_ptr(value).as_ptr() as *const c_void);
        alive.push(value.clone());
    }
    // SAFETY: both slices are the same length and hold live Core Foundation objects; the dictionary
    // retains what it is given.
    unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            keys.len() as isize,
            std::ptr::null(),
            std::ptr::null(),
        )
    }
    .ok_or_else(|| anyhow!("could not build a Core Foundation dictionary"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder is real hardware, and this checks the whole path in one go: a session opens, a frame
    /// goes in, and an Annex-B picture with its parameter sets in front comes out.
    #[test]
    fn a_frame_encodes_to_an_annexb_keyframe() {
        use objc2_core_video::{
            kCVPixelFormatType_32BGRA, CVPixelBuffer, CVPixelBufferCreate,
            CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
            CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        };

        const W: usize = 640;
        const H: usize = 360;

        let mut encoder = match Encoder::new(W as i32, H as i32, 30, 4_000) {
            Ok(encoder) => encoder,
            // A machine with no hardware encoder is not a failing test; there is nothing to check.
            Err(err) => {
                eprintln!("no encoder here: {err}");
                return;
            }
        };

        // A BGRA buffer with something in it. A flat colour compresses to almost nothing, which would
        // still prove the path but not that the size is plausible.
        let mut buffer: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(None, W, H, kCVPixelFormatType_32BGRA, None, NonNull::new(&mut buffer).unwrap())
        };
        assert_eq!(status, 0, "could not make a pixel buffer");
        let pixels = unsafe { CFRetained::from_raw(NonNull::new(buffer).unwrap()) };

        unsafe {
            CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::empty());
            let base = CVPixelBufferGetBaseAddress(&pixels) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRow(&pixels);
            for y in 0..H {
                for x in 0..W {
                    let at = base.add(y * stride + x * 4);
                    *at = (x % 256) as u8;
                    *at.add(1) = (y % 256) as u8;
                    *at.add(2) = 128;
                    *at.add(3) = 255;
                }
            }
            CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::empty());
        }

        encoder.encode(&pixels, true).expect("the frame should be accepted");

        // The callback is asynchronous, so wait for it rather than assuming.
        let mut picture = None;
        for _ in 0..200 {
            if let Some(found) = encoder.take() {
                picture = Some(found);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let picture = picture.expect("an encoded picture within two seconds");

        assert!(picture.keyframe, "a forced keyframe should be reported as one");
        assert_eq!(&picture.data[..4], &[0, 0, 0, 1], "Annex-B starts with a start code");
        assert!(picture.data.len() > 200, "suspiciously small: {}", picture.data.len());

        // The parameter sets have to be in front of it, or a decoder joining here has nothing to
        // decode against. SPS is NAL type 7.
        let first_nal = picture.data[4] & 0x1F;
        assert_eq!(first_nal, 7, "a keyframe must lead with its SPS, got NAL type {first_nal}");
        // And somewhere after it, a PPS (8) and an IDR slice (5).
        let types: Vec<u8> = picture
            .data
            .windows(5)
            .filter(|w| w[..4] == [0, 0, 0, 1])
            .map(|w| w[4] & 0x1F)
            .collect();
        assert!(types.contains(&8), "no PPS: {types:?}");
        assert!(types.contains(&5), "no IDR slice: {types:?}");
    }
}
