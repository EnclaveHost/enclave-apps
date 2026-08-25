//! Watching the machine, moved off the machine's thread.
//!
//! Everything this app does has always run in one loop: the emulator ticks,
//! then the framebuffer is scanned and deflated, then AV1 encodes a frame,
//! then HTTP is served. That is not four things sharing a machine, it is four
//! things sharing ONE CORE, because a wasip2 component cannot spawn a thread
//! (`std::thread::spawn` -> os error 58, on p2 and p3 alike). So watching the
//! machine has always been paid for by the machine: measured, the AV1 stream
//! cost **82%** of guest speed (36.3 -> 6.6 MIPS) and the deflate band stream
//! costs a fifth of the thread at its budget. Both are throttled in the main
//! loop for exactly that reason.
//!
//! Shared-everything-threads changes the arithmetic. `thread.spawn-indirect`
//! runs guest code on a REAL second core inside the same component instance,
//! over the same linear memory, so the expensive half can simply leave.
//!
//! Two constraints shape the design, and neither is negotiable:
//!
//! 1. **The emulator cannot leave the main thread.** `Emulator` is the whole
//!    machine and the HTTP handlers mutate it. So the split is not "move the
//!    display code" — it is "the emulator's thread does the one thing that
//!    needs the emulator (a memcpy of the framebuffer out of guest RAM) and
//!    the worker does everything else": hashing, diffing, deflating, RGB
//!    conversion, AV1.
//!
//! 2. **A worker cannot touch a socket.** SET gives every thread its own fd
//!    namespace (`fd = namespace << 13 | index`); a descriptor opened on the
//!    main thread is `EBADF` on a worker, deliberately. So the worker never
//!    writes to a client. It returns BYTES, and the main thread broadcasts
//!    them — which is fine, because framing an SSE event is nothing next to
//!    the compression that produced it.
//!
//! The result is a one-job-in-flight pipeline with a two-buffer pool, so a
//! steady state allocates nothing: the main thread captures into a spare
//! buffer, hands it over, and gets the previous frame back to capture into
//! next time.
//!
//! When SET is not available — the ordinary wasip2 component, which is what
//! ships today — `start()` returns false and every caller falls back to doing
//! the work inline. Same code path, same output, just paid for by the guest.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::display::{self, Display};
use crate::video::{self, EncodedFrame, VideoEncoder};

/// Which codec the video stream wants, set by the `/video` handler when a
/// watcher joins and read wherever the encoder is (re)built — over here on the
/// worker, or inline on the main thread when there is no worker. Packed as
/// codec in the low byte (0 = AV1, 1 = H.264) and kbps above it, one atomic so
/// a torn read cannot pair one codec with the other's bitrate.
static VIDEO_PARAMS: AtomicU32 = AtomicU32::new(0);
/// A client asked for a random-access point (Moonlight lost packets).
static VIDEO_FORCE_KEY: AtomicBool = AtomicBool::new(false);

pub const CODEC_AV1: u8 = 0;
pub const CODEC_H264: u8 = 1;

pub fn set_video_params(codec: u8, kbps: u32) {
    VIDEO_PARAMS.store((kbps.min(50_000) << 8) | codec as u32, Ordering::Release);
}

pub fn video_params() -> (u8, u32) {
    let v = VIDEO_PARAMS.load(Ordering::Acquire);
    ((v & 0xff) as u8, v >> 8)
}

/// The raw packed value, for change detection against a built encoder.
pub fn packed_params() -> u32 {
    VIDEO_PARAMS.load(Ordering::Acquire)
}

pub fn force_key() {
    VIDEO_FORCE_KEY.store(true, Ordering::Release);
}

pub fn take_force_key() -> bool {
    VIDEO_FORCE_KEY.swap(false, Ordering::AcqRel)
}

/// The frame rate the hardware encoder is configured for. The capture loop is
/// paced separately (see the cadence ceiling below); this is what the card is
/// told to expect so its rate control targets the right bitrate per frame.
const TARGET_FPS: u32 = 60;

/// Cached answer to "can this host encode H.264 on the GPU?".
///
/// `nvenc::caps()` loads a graph and opens an execution context, so it must not
/// run per viewer. 0 = unknown, 1 = yes, 2 = no.
static NVENC_OK: AtomicU32 = AtomicU32::new(0);

fn nvenc_supported() -> bool {
    match NVENC_OK.load(Ordering::Acquire) {
        1 => true,
        2 => false,
        _ => {
            let ok = crate::nvenc::available();
            NVENC_OK.store(if ok { 1 } else { 2 }, Ordering::Release);
            eprintln!(
                "[nvenc] hardware H.264 {}",
                if ok { "available - encoding on the GPU" } else { "unavailable - software encode" }
            );
            ok
        }
    }
}

/// Build the encoder the current params ask for. Shared by the worker loop and
/// the inline fallback so both agree on defaults.
pub fn build_encoder() -> Option<(u32, Box<dyn VideoEncoder + Send>)> {
    let (codec, kbps) = video_params();
    let (w, h) = (display::fb_w(), display::fb_h());
    let params = VIDEO_PARAMS.load(Ordering::Acquire);
    match codec {
        CODEC_H264 => {
            let kbps = if kbps == 0 { 3000 } else { kbps };
            // Hardware first. A gpuShare deployment on a GPU enclave whose
            // toolchain carries the nvenc backend encodes on the card's
            // fixed-function block; everything else falls through to minih264
            // in-wasm. Both produce Annex-B H.264, so the client never learns
            // which one it got -- the only difference it can see is that the
            // hardware path honours a mid-stream IDR request and holds 60 fps
            // where software tops out around 43 (PLATFORM-ENCODE.md).
            //
            // The probe is cached: it opens a graph and a context, which is far
            // too expensive to repeat every time a viewer joins.
            if nvenc_supported() {
                if let Some(e) = crate::nvenc::NvencEncoder::new(w, h, TARGET_FPS, kbps) {
                    return Some((params, Box::new(e) as Box<dyn VideoEncoder + Send>));
                }
                // Opening a session can fail on a card that is out of NVENC
                // slots even though the backend is present. Say so once and
                // carry on in software rather than dropping the stream.
                eprintln!("[nvenc] no session available; falling back to minih264");
            }
            video::H264Encoder::new(w, h, kbps)
                .map(|e| (params, Box::new(e) as Box<dyn VideoEncoder + Send>))
        }
        _ => {
            let kbps = if kbps == 0 { 4000 } else { kbps };
            video::Av1Encoder::new(w, h, kbps as i32 * 1000, 10)
                .map(|e| (params, Box::new(e) as Box<dyn VideoEncoder + Send>))
        }
    }
}

/// Ceiling on the encode cadence. The capture loop runs at the display scan
/// floor (8–16 ms), but 60–120 encodes a second would just spread the bitrate
/// thinner and burn the worker core; the target is a stable 30 fps stream, so
/// cap a little above it and let per-frame quality keep the headroom.
// 60 fps is the target the stream is judged against, so the floor has to
// admit it: at 25 ms the pacing itself capped a perfect machine at 40.
// 14 ms leaves headroom for turn jitter so the delivered rate lands at or
// above 60 rather than just under it; the capture cost and the encoder are
// then the only real limits.
pub const VIDEO_MIN_INTERVAL: Duration = Duration::from_millis(14);

/// One captured framebuffer, plus what the watchers currently want done to it.
pub struct Job {
    pub frame: Vec<u8>,
    pub want_bands: bool,
    pub want_video: bool,
    /// The row range the guest said it changed, taken at capture time. None
    /// means "no damage report" — scan the whole frame, as before.
    pub damage: Option<(usize, usize)>,
}

/// What came back. `spare` is the buffer to capture into next.
pub struct Out {
    pub bands: Vec<display::Band>,
    pub video: Vec<EncodedFrame>,
    pub spare: Vec<u8>,
    /// Wall time the worker spent. Only used for reporting: with a worker this
    /// is no longer a budget the guest pays, which is the entire point.
    pub cost: Duration,
}

#[derive(Default)]
struct Pipe {
    jobs: Mutex<VecDeque<Job>>,
    outs: Mutex<VecDeque<Out>>,
    spare: Mutex<Vec<Vec<u8>>>,
    /// A watcher joined: the next band pass must ship a whole frame.
    force_full: AtomicBool,
    /// The machine stopped or rebooted: drop the diff state.
    reset: AtomicBool,
    running: AtomicBool,
    inflight: AtomicU32,
}

fn pipe() -> &'static Pipe {
    static PIPE: OnceLock<Pipe> = OnceLock::new();
    PIPE.get_or_init(Pipe::default)
}

/// True once a worker is running and offloading is worth attempting.
pub fn available() -> bool {
    pipe().running.load(Ordering::Acquire)
}

/// Jobs handed over but not yet collected. The caller uses this to keep
/// exactly one frame in flight: capturing faster than the worker can compress
/// would just grow a queue of stale screens.
pub fn inflight() -> u32 {
    pipe().inflight.load(Ordering::Acquire)
}

/// A buffer to capture into, recycled if one is going spare.
pub fn take_buffer() -> Vec<u8> {
    pipe().spare.lock().unwrap().pop().unwrap_or_default()
}

pub fn submit(job: Job) {
    let p = pipe();
    p.inflight.fetch_add(1, Ordering::AcqRel);
    p.jobs.lock().unwrap().push_back(job);
}

pub fn collect() -> Option<Out> {
    let p = pipe();
    let out = p.outs.lock().unwrap().pop_front();
    if out.is_some() {
        p.inflight.fetch_sub(1, Ordering::AcqRel);
    }
    out
}

/// Give a buffer back to the pool. Two is all a one-in-flight pipeline can
/// use, so the pool is capped there rather than growing without bound.
pub fn recycle(mut buf: Vec<u8>) {
    let mut pool = pipe().spare.lock().unwrap();
    if pool.len() < 2 {
        buf.clear();
        pool.push(buf);
    }
}

pub fn want_full() {
    pipe().force_full.store(true, Ordering::Release);
}

pub fn reset() {
    pipe().reset.store(true, Ordering::Release);
}

/// The worker's own loop. Owns the diff state and the encoder, because both
/// are per-stream state that only it touches. Runs for the life of the
/// component: there is no stop path because there is nothing to stop for — a
/// machine that halts just stops producing jobs.
#[cfg_attr(not(feature = "set"), allow(dead_code))]
fn serve() {
    let p = pipe();
    let mut display = Display::new();
    // The encoder plus the params it was built with, so a watcher switching
    // codec or bitrate rebuilds it (and the rebuild's first frame is an IDR).
    let mut enc: Option<(u32, Box<dyn VideoEncoder + Send>)> = None;
    let mut next_encode: Option<Instant> = None;

    loop {
        let job = p.jobs.lock().unwrap().pop_front();
        let Some(job) = job else {
            // Nothing to do. A short sleep rather than a spin: this thread is
            // a real core, and burning it while the screen is still would be
            // the same mistake in a new place.
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };

        let began = Instant::now();
        if p.reset.swap(false, Ordering::AcqRel) {
            display.reset();
            enc = None;
        }
        if p.force_full.swap(false, Ordering::AcqRel) {
            display.want_full();
        }

        let mut video = Vec::new();
        if job.want_video {
            let params = packed_params();
            if enc.as_ref().map(|(p, _)| *p) != Some(params) {
                enc = build_encoder();
                next_encode = None;
            }
            // An absolute schedule, not "interval since the last encode":
            // jobs arrive on the capture clock (~16 ms), and gating each one
            // on a 25 ms elapsed check quantizes the cadence to every other
            // job — 32 ms, 31 fps, measured as 27-28 on the wire. Advancing a
            // deadline by the interval instead absorbs the beat: encodes land
            // on the first job past each deadline and average the true 40.
            let now = Instant::now();
            let due = match next_encode {
                None => true,
                Some(at) => now >= at,
            };
            if due {
                if let Some((_, e)) = enc.as_mut() {
                    if take_force_key() {
                        e.force_keyframe();
                    }
                    video = e.encode_capture(&job.frame).unwrap_or_else(|| {
                        let (rgb, w, h) = video::rgb_from_capture(&job.frame);
                        e.encode(&rgb, w, h)
                    });
                    next_encode = Some(match next_encode {
                        // Catch up at most one interval; a long stall must not
                        // bank a burst of instantly-due encodes.
                        Some(at) if now < at + VIDEO_MIN_INTERVAL => at + VIDEO_MIN_INTERVAL,
                        _ => now + VIDEO_MIN_INTERVAL,
                    });
                }
            }
        } else if enc.is_some() {
            enc = None;
        }

        // Bands last: it consumes the frame and hands back the buffer.
        let (bands, spare) = match job.want_bands {
            true => display.bands(job.frame, job.damage),
            false => (Vec::new(), job.frame),
        };

        p.outs.lock().unwrap().push_back(Out { bands, video, spare, cost: began.elapsed() });
    }
}

#[cfg(feature = "set")]
mod spawn {
    use std::os::raw::{c_int, c_void};

    extern "C" {
        fn pthread_create(
            thread: *mut usize,
            attr: *const c_void,
            start: extern "C" fn(*mut c_void) -> *mut c_void,
            arg: *mut c_void,
        ) -> c_int;
    }

    extern "C" fn entry(_: *mut c_void) -> *mut c_void {
        super::serve();
        std::ptr::null_mut()
    }

    /// Spawn through the SET libc's pthreads, which route to the component
    /// model's `thread.spawn-indirect` builtin. Note this is NOT
    /// `std::thread::spawn`: Rust's std threading targets wasi-threads' host
    /// import, a different mechanism that this engine does not serve.
    pub fn worker() -> bool {
        let mut tid: usize = 0;
        let rc = unsafe { pthread_create(&mut tid, std::ptr::null(), entry, std::ptr::null_mut()) };
        rc == 0
    }
}

#[cfg(not(feature = "set"))]
mod spawn {
    /// Without the SET toolchain there is no thread to spawn — a wasip2
    /// component is one core and `pthread_create` would not even link.
    pub fn worker() -> bool {
        false
    }
}

/// Try to bring a worker up. False means every caller should keep doing the
/// work inline, which is exactly what this app did before.
pub fn start() -> bool {
    let p = pipe();
    if p.running.load(Ordering::Acquire) {
        return true;
    }
    if !spawn::worker() {
        eprintln!("[risc-box] no display worker: watching the machine costs the machine");
        return false;
    }
    p.running.store(true, Ordering::Release);
    eprintln!("[risc-box] display worker running on its own core");
    true
}
