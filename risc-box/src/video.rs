//! Video encode path: turn the guest's framebuffer into a real video-codec
//! bitstream, encoded **in this app** — not in the emulated guest.
//!
//! This is the load-bearing architectural decision for streaming RISC Box (see
//! `docs/streaming.md`): the emulated RISC-V CPU runs at ~20–30 MIPS, so an
//! encoder *inside the guest* (Sunshine/x264) manages well under a frame per
//! second. But this app — the wasm32-wasip2 host that owns the emulator — runs
//! under wasmtime's JIT at near-native speed and has direct, native-speed
//! access to the guest framebuffer (it's host RAM; `read_physical_range` is a
//! slice copy). So capture + encode + stream belong here. The guest only has
//! to run the desktop and take input (the virtio-input HID). That difference
//! is the whole reason streaming is tractable.
//!
//! The [`VideoEncoder`] trait is the seam. Backends, cheapest to fastest:
//!   - [`MjpegEncoder`] — pure-Rust Motion JPEG, here now, needs nothing.
//!   - a software H.264/HEVC backend (openh264/x264 compiled into the wasm) —
//!     inter-frame, far smaller bitstream, still CPU (app-side, so real-time
//!     at this resolution).
//!   - an **NVENC backend that offloads to the H200** — the app hands raw
//!     frames to a host-side encode call (a wasi-nn-shaped addition to the
//!     toolchain, or a native GPU sidecar in the CVM) and gets H.264/HEVC/AV1
//!     packets back. The emulated CPU is out of the encode loop entirely; the
//!     only per-frame cost here is the framebuffer capture memcpy. This is the
//!     "make it as fast as possible with the H200" path.
//!
//! All backends share the capture front-end ([`capture_rgb`]) and the
//! [`VideoEncoder`] interface, so moving from Motion JPEG to NVENC is a backend
//! swap, not a rewrite of the capture or the app plumbing.

use riscv_emu_rust::Emulator;

use crate::display::{fb_bytes, fb_h, fb_stride, fb_w, FB_BASE};

/// One encoded frame and whether it is a keyframe (self-contained). Motion
/// JPEG frames are all keyframes; an inter-frame codec (H.264) marks only IDR
/// frames, which the transport uses to let a late client start cleanly.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// A pluggable video encoder over the guest's RGB framebuffer. Backends range
/// from pure-Rust Motion JPEG (intra-only) to AV1 (inter-frame, rav1e) to an
/// H.264 encoder offloading to the H200's NVENC (see the module docs).
pub trait VideoEncoder {
    /// Encode one RGB frame (`rgb` is `width*height*3`, row-major, no padding).
    /// Returns zero or more coded frames: an intra codec yields exactly one; a
    /// stateful inter-frame codec (AV1) may buffer, so it can yield zero (needs
    /// more input) or occasionally more than one.
    fn encode(&mut self, rgb: &[u8], width: usize, height: usize) -> Vec<EncodedFrame>;
    /// The MIME type of the produced bitstream, for the HTTP transport.
    fn mime(&self) -> &'static str;
    /// The WebCodecs codec string for the browser `VideoDecoder`, or "" for a
    /// container the browser plays directly (e.g. Motion JPEG via `<img>`).
    fn webcodec(&self) -> &'static str {
        ""
    }
    /// Ask for the next frame to be a random-access point (a Moonlight client
    /// that lost packets requests one). Default: no-op; codecs that cannot
    /// force one mid-stream are handled by dropping and rebuilding the encoder.
    fn force_keyframe(&mut self) {}
    /// Encode straight from the captured BGRX framebuffer, skipping the packed
    /// RGB intermediate — one pass over the pixels instead of three, which is
    /// a real fraction of the per-frame budget at 1024x768 under wasm. `None`
    /// means the backend has no fast path and the caller should convert and
    /// call [`VideoEncoder::encode`].
    fn encode_capture(&mut self, _fresh: &[u8]) -> Option<Vec<EncodedFrame>> {
        None
    }
}

/// Convert packed RGB to planar I420 (YUV 4:2:0, BT.601 limited range) — the
/// input AV1/H.264 encoders want. Returns (y, u, v) planes; chroma is
/// `((w+1)/2) * ((h+1)/2)`.
pub fn rgb_to_i420(rgb: &[u8], w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let cw = (w + 1) / 2;
    let ch = (h + 1) / 2;
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; cw * ch];
    let mut v = vec![0u8; cw * ch];
    for j in 0..h {
        for i in 0..w {
            let p = (j * w + i) * 3;
            let (r, g, b) = (rgb[p] as i32, rgb[p + 1] as i32, rgb[p + 2] as i32);
            // BT.601 limited-range luma
            y[j * w + i] = ((66 * r + 129 * g + 25 * b + 128) >> 8).clamp(0, 219) as u8 + 16;
            // subsample chroma from the top-left pixel of each 2x2 block
            if (j & 1) == 0 && (i & 1) == 0 {
                let ci = (j / 2) * cw + i / 2;
                u[ci] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
                v[ci] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            }
        }
    }
    (y, u, v)
}

/// Capture the current guest framebuffer as packed RGB (drops the X byte and
/// reorders the guest's B,G,R,X to R,G,B). This is the shared front-end for
/// every encoder backend; it is the *only* per-frame work the emulated CPU is
/// never involved in — a native-speed copy out of guest RAM plus a channel
/// swap. Returns `(rgb, w, h)`.
pub fn capture_rgb(emu: &Emulator) -> (Vec<u8>, usize, usize) {
    let mut fresh = vec![0u8; fb_bytes()];
    emu.read_physical_range(FB_BASE, &mut fresh);
    let (rgb, w, h) = rgb_from_capture(&fresh);
    (rgb, w, h)
}

/// The same conversion against a framebuffer someone else already copied out
/// of guest RAM. Splitting it out is what lets the display and video paths
/// share ONE capture: the emulator's thread does a single memcpy, and both the
/// band diff and the encoder work from that copy — on another thread, where
/// neither of them is charged to the guest.
pub fn rgb_from_capture(fresh: &[u8]) -> (Vec<u8>, usize, usize) {
    let mut rgb = Vec::with_capacity(fb_w() * fb_h() * 3);
    for y in 0..fb_h() {
        let row = &fresh[y * fb_stride()..(y + 1) * fb_stride()];
        for px in row.chunks_exact(4) {
            rgb.push(px[2]); // R
            rgb.push(px[1]); // G
            rgb.push(px[0]); // B
        }
    }
    (rgb, fb_w(), fb_h())
}

/// Motion JPEG: each frame is an independent baseline JPEG. The first real
/// video codec on the path — intra-only (every frame is a keyframe, so it is
/// bandwidth-heavy vs. H.264 but has zero inter-frame state), pure Rust, and
/// wasm-friendly. Encoding runs app-side at JIT speed, so an 800x600 frame is
/// milliseconds, not the seconds it would take in the guest.
pub struct MjpegEncoder {
    quality: u8,
}

impl MjpegEncoder {
    pub fn new(quality: u8) -> Self {
        MjpegEncoder { quality: quality.clamp(1, 100) }
    }
}

impl VideoEncoder for MjpegEncoder {
    fn encode(&mut self, rgb: &[u8], width: usize, height: usize) -> Vec<EncodedFrame> {
        let mut buf = Vec::new();
        let encoder = jpeg_encoder::Encoder::new(&mut buf, self.quality);
        // Infallible for an in-memory sink with a correctly-sized RGB buffer.
        let _ = encoder.encode(rgb, width as u16, height as u16, jpeg_encoder::ColorType::Rgb);
        vec![EncodedFrame { data: buf, keyframe: true }]
    }
    fn mime(&self) -> &'static str {
        "image/jpeg"
    }
}

/// AV1 via rav1e — the efficient, inter-frame codec. Stateful (holds the
/// encoder context across frames), so a mostly-static desktop costs a handful
/// of bytes per frame. Encodes app-side at wasmtime-JIT speed (~10–20 fps at
/// 800x600). The browser decodes it with WebCodecs; the same coded frames
/// could feed a Moonlight/RTP transport later. (Note: Moonlight itself wants
/// H.264/HEVC, not AV1 — for that, the NVENC/H.264 backend in the module docs
/// is the path; AV1 is the pure-Rust codec that runs efficiently *here*.)
pub struct Av1Encoder {
    ctx: rav1e::Context<u8>,
    w: usize,
    h: usize,
}

impl Av1Encoder {
    /// `bitrate` in bits/sec; `speed` 0..=10 (10 = fastest, what we want).
    pub fn new(w: usize, h: usize, bitrate: i32, speed: u8) -> Option<Self> {
        use rav1e::prelude::*;
        let enc = EncoderConfig {
            width: w,
            height: h,
            bit_depth: 8,
            chroma_sampling: ChromaSampling::Cs420,
            speed_settings: SpeedSettings::from_preset(speed.min(10)),
            low_latency: true,
            bitrate,
            min_key_frame_interval: 60,
            max_key_frame_interval: 300,
            ..Default::default()
        };
        let cfg = Config::new().with_encoder_config(enc);
        let ctx = cfg.new_context().ok()?;
        Some(Av1Encoder { ctx, w, h })
    }
}

impl VideoEncoder for Av1Encoder {
    fn encode(&mut self, rgb: &[u8], _width: usize, _height: usize) -> Vec<EncodedFrame> {
        use rav1e::prelude::*;
        let (y, u, v) = rgb_to_i420(rgb, self.w, self.h);
        let mut frame = self.ctx.new_frame();
        let planes: [&[u8]; 3] = [&y, &u, &v];
        // copy_from_raw_u8's stride arg is the SOURCE row stride (how wide each
        // row is in `src`), NOT the plane's padded internal stride — luma rows
        // are `w` wide, chroma rows `(w+1)/2`. Passing the padded dst stride
        // here shears detailed regions (a classic stride bug).
        let cw = (self.w + 1) / 2;
        let src_strides = [self.w, cw, cw];
        for ((p, src), &ss) in frame.planes.iter_mut().zip(planes).zip(&src_strides) {
            p.copy_from_raw_u8(src, ss, 1);
        }
        if self.ctx.send_frame(frame).is_err() {
            return vec![];
        }
        let mut out = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(pkt) => out.push(EncodedFrame {
                    keyframe: pkt.frame_type == FrameType::KEY,
                    data: pkt.data,
                }),
                Err(EncoderStatus::Encoded) => continue,
                _ => break, // NeedMoreData / LimitReached / other: done for now
            }
        }
        out
    }
    fn mime(&self) -> &'static str {
        "video/AV01"
    }
    fn webcodec(&self) -> &'static str {
        // profile 0 (Main), level 4.0, Main tier, 8-bit
        "av01.0.08M.08"
    }
}

/// H.264 via minih264 (vendor/minih264, CC0) — the codec a Moonlight client
/// decodes natively, encoded fast enough to matter: measured 2.7 ms/frame at
/// 1024x768 native (374 fps), which leaves real-time 30 fps intact even after
/// the wasm and slower-fleet-core discounts. rav1e at the same size measured
/// 2.9 fps on the fleet, so AV1 stays the browser/WebCodecs codec and this is
/// the streaming one. The C side is two flat entry points over caller-owned
/// buffers (no allocation in C); see vendor/minih264/wrapper.c.
extern "C" {
    fn rbx_h264_sizeof(w: i32, h: i32, gop: i32, kbps: i32, persist: *mut i32, scratch: *mut i32) -> i32;
    fn rbx_h264_init(persist: *mut u8, w: i32, h: i32, gop: i32, kbps: i32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn rbx_h264_encode(
        persist: *mut u8,
        scratch: *mut u8,
        y: *const u8,
        u: *const u8,
        v: *const u8,
        w: i32,
        h: i32,
        force_key: i32,
        desired_bytes: i32,
        qp_min: i32,
        qp_max: i32,
        coded: *mut *const u8,
        coded_len: *mut i32,
    ) -> i32;
}

/// Keyframe period. Long on purpose: every joiner gets a fresh encoder (so an
/// IDR leads its stream), and a client that loses packets asks for one via
/// `force_keyframe`, so the periodic IDR is only a backstop.
const H264_GOP: i32 = 300;

pub struct H264Encoder {
    persist: Vec<u64>, // u64-backed so the C state is 8-aligned
    scratch: Vec<u64>,
    w: usize,
    h: usize,
    kbps: u32,
    force_key: bool,
}

impl H264Encoder {
    /// `kbps` is the VBV-controlled target bitrate. Dimensions must be
    /// macroblock-aligned (multiples of 16) — the framebuffer's 1024x768 is.
    pub fn new(w: usize, h: usize, kbps: u32) -> Option<Self> {
        if w == 0 || h == 0 || w % 16 != 0 || h % 16 != 0 {
            return None;
        }
        let (mut p, mut s) = (0i32, 0i32);
        let rc = unsafe { rbx_h264_sizeof(w as i32, h as i32, H264_GOP, kbps as i32, &mut p, &mut s) };
        if rc != 0 || p <= 0 || s <= 0 {
            return None;
        }
        let mut persist = vec![0u64; (p as usize).div_ceil(8)];
        let scratch = vec![0u64; (s as usize).div_ceil(8)];
        let rc = unsafe {
            rbx_h264_init(persist.as_mut_ptr() as *mut u8, w as i32, h as i32, H264_GOP, kbps as i32)
        };
        if rc != 0 {
            return None;
        }
        Some(H264Encoder { persist, scratch, w, h, kbps, force_key: false })
    }
}

/// One-pass BGRX (the guest framebuffer's layout, `stride` bytes a row) to
/// I420, BT.601 limited range — the same math as [`rgb_to_i420`] without the
/// packed-RGB intermediate or its extra pass. Dimensions must be even.
pub fn bgrx_to_i420(fresh: &[u8], w: usize, h: usize, stride: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let cw = w / 2;
    let mut y = vec![0u8; w * h];
    let mut u = vec![0u8; cw * (h / 2)];
    let mut v = vec![0u8; cw * (h / 2)];
    for j in 0..h {
        let row = &fresh[j * stride..j * stride + w * 4];
        let yrow = &mut y[j * w..(j + 1) * w];
        if j & 1 == 0 {
            let urow = &mut u[(j / 2) * cw..(j / 2) * cw + cw];
            let vrow = &mut v[(j / 2) * cw..(j / 2) * cw + cw];
            for (i2, px) in row.chunks_exact(8).enumerate() {
                let (b0, g0, r0) = (px[0] as i32, px[1] as i32, px[2] as i32);
                let (b1, g1, r1) = (px[4] as i32, px[5] as i32, px[6] as i32);
                yrow[i2 * 2] = (((66 * r0 + 129 * g0 + 25 * b0 + 128) >> 8).clamp(0, 219) + 16) as u8;
                yrow[i2 * 2 + 1] = (((66 * r1 + 129 * g1 + 25 * b1 + 128) >> 8).clamp(0, 219) + 16) as u8;
                // chroma from the top-left pixel of each 2x2 block, as before
                urow[i2] = ((((-38 * r0 - 74 * g0 + 112 * b0 + 128) >> 8) + 128).clamp(16, 240)) as u8;
                vrow[i2] = ((((112 * r0 - 94 * g0 - 18 * b0 + 128) >> 8) + 128).clamp(16, 240)) as u8;
            }
        } else {
            for (i, px) in row.chunks_exact(4).enumerate() {
                let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
                yrow[i] = (((66 * r + 129 * g + 25 * b + 128) >> 8).clamp(0, 219) + 16) as u8;
            }
        }
    }
    (y, u, v)
}

impl H264Encoder {
    fn encode_planes(&mut self, y: &[u8], u: &[u8], v: &[u8]) -> Vec<EncodedFrame> {
        let mut coded: *const u8 = std::ptr::null();
        let mut coded_len: i32 = 0;
        let force = std::mem::take(&mut self.force_key);
        let rc = unsafe {
            rbx_h264_encode(
                self.persist.as_mut_ptr() as *mut u8,
                self.scratch.as_mut_ptr() as *mut u8,
                y.as_ptr(),
                u.as_ptr(),
                v.as_ptr(),
                self.w as i32,
                self.h as i32,
                force as i32,
                (self.kbps as i32) * 1000 / 8 / 30, // per-frame budget at 30 fps
                10,
                48,
                &mut coded,
                &mut coded_len,
            )
        };
        if rc != 0 || coded.is_null() || coded_len <= 0 {
            return vec![];
        }
        let data = unsafe { std::slice::from_raw_parts(coded, coded_len as usize) }.to_vec();
        // A random-access frame is recognizable by its access unit leading
        // with an SPS NAL (type 7) — the same rule Moonlight applies.
        let keyframe = data.len() > 4 && data[..4] == [0, 0, 0, 1] && data[4] & 0x1f == 7
            || data.len() > 3 && data[..3] == [0, 0, 1] && data[3] & 0x1f == 7;
        vec![EncodedFrame { data, keyframe }]
    }
}

impl VideoEncoder for H264Encoder {
    fn encode(&mut self, rgb: &[u8], _width: usize, _height: usize) -> Vec<EncodedFrame> {
        let (y, u, v) = rgb_to_i420(rgb, self.w, self.h);
        self.encode_planes(&y, &u, &v)
    }
    fn encode_capture(&mut self, fresh: &[u8]) -> Option<Vec<EncodedFrame>> {
        if fresh.len() < fb_bytes() || (self.w, self.h) != (fb_w(), fb_h()) {
            return None;
        }
        let (y, u, v) = bgrx_to_i420(fresh, self.w, self.h, fb_stride());
        Some(self.encode_planes(&y, &u, &v))
    }
    fn mime(&self) -> &'static str {
        "video/H264"
    }
    fn webcodec(&self) -> &'static str {
        // Constrained Baseline, level 3.2 (Annex-B stream; a WebCodecs
        // consumer must configure `avc: {format: "annexb"}`).
        "avc1.42E020"
    }
    fn force_keyframe(&mut self) {
        self.force_key = true;
    }
}
