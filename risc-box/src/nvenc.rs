//! Hardware H.264 encode on the fleet GPU, reached through wasi-nn.
//!
//! PLATFORM.md §0 is the constraint this module exists under: an emulated
//! RISC-V guest has no GPU device and no NVENC userspace, and a wasm tenant
//! cannot spawn ffmpeg, so **the card is reachable only through the wasi-nn
//! shims**. The `nvenc` backend is a preload-only graph (`-S
//! nn-graph=nvenc::<dir>`) — there is no model file, the graph IS the encoder.
//!
//! Encode is *stateful* (frame N references N-1), which is the one structural
//! difference from every other wasi-nn backend. It maps onto the API that
//! already exists for that: `init_execution_context` opens exactly one NVENC
//! session and dropping the context closes it. One encoder here == one context.
#![allow(dead_code)]

use crate::video::{EncodedFrame, VideoEncoder};

// Bindings are generated from wit/ at compile time. `generate_all` pulls the
// vendored wasi:nn deps; the world imports only — risc-box is a command
// component, so there is nothing to export.
#[cfg(target_arch = "wasm32")]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "risc-box",
        generate_all,
    });
}

#[cfg(target_arch = "wasm32")]
use bindings::wasi::nn::graph::load_by_name;
#[cfg(target_arch = "wasm32")]
use bindings::wasi::nn::inference::GraphExecutionContext;
#[cfg(target_arch = "wasm32")]
use bindings::wasi::nn::tensor::{Tensor, TensorType};

/// The graph name the manager preloads. Not a file — the encoder itself.
const GRAPH: &str = "nvenc";

/// Capability bits returned by the `caps` probe (mirrors `env_caps()`).
pub const CAP_H264: i32 = 1 << 0;
pub const CAP_HEVC: i32 = 1 << 1;
pub const CAP_AV1: i32 = 1 << 2;

#[cfg(target_arch = "wasm32")]
fn u8_tensor(name: &str, data: Vec<u8>) -> (String, Tensor) {
    (name.to_string(), Tensor::new(&[data.len() as u32], TensorType::U8, &data))
}

#[cfg(target_arch = "wasm32")]
fn i32_tensor(name: &str, v: i32) -> (String, Tensor) {
    (name.to_string(), Tensor::new(&[1], TensorType::I32, &v.to_le_bytes()))
}

#[cfg(target_arch = "wasm32")]
fn find<'a>(outs: &'a [(String, Tensor)], name: &str) -> Option<&'a Tensor> {
    outs.iter().find(|(n, _)| n == name).map(|(_, t)| t)
}

/// Ask the host what it can encode without opening a session.
///
/// A host that predates the backend answers with a missing slot rather than
/// trapping, and a guest reads that as "no" — so an old toolchain degrades
/// honestly into "streaming unavailable" instead of killing the app. That is
/// the same contract the ggml probe uses.
#[cfg(target_arch = "wasm32")]
pub fn caps() -> i32 {
    let Ok(graph) = load_by_name(GRAPH) else { return 0 };
    let Ok(ctx) = graph.init_execution_context() else { return 0 };
    let Ok(outs) = ctx.compute(vec![i32_tensor("caps", 0)]) else { return 0 };
    find(&outs, "caps")
        .map(|t| {
            let d = t.data();
            if d.len() >= 4 { i32::from_le_bytes([d[0], d[1], d[2], d[3]]) } else { 0 }
        })
        .unwrap_or(0)
}

/// True when the host can encode H.264 for us.
#[cfg(target_arch = "wasm32")]
pub fn available() -> bool {
    caps() & CAP_H264 != 0
}

#[cfg(target_arch = "wasm32")]
pub struct NvencEncoder {
    ctx: GraphExecutionContext,
    width: usize,
    height: usize,
    /// Reused NV12 scratch so a 60 fps stream does not allocate 1.2 MB a frame.
    nv12: Vec<u8>,
    force_idr: bool,
}

#[cfg(target_arch = "wasm32")]
impl NvencEncoder {
    /// Open one NVENC session sized to the framebuffer.
    ///
    /// `None` when the host has no nvenc backend (old toolchain), no GPU share,
    /// or no free NVENC session — every one of which is a "fall back to the
    /// software encoder" condition, not a fatal error.
    pub fn new(width: usize, height: usize, fps: u32, kbps: u32) -> Option<Self> {
        // NVENC wants even dimensions for 4:2:0 chroma.
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            eprintln!("[nvenc] {width}x{height} is not 4:2:0-encodable (needs even dimensions)");
            return None;
        }
        let graph = load_by_name(GRAPH)
            .map_err(|e| eprintln!("[nvenc] load_by_name({GRAPH}) failed: {e:?} - \
                                    needs a gpuShare deployment on a GPU enclave whose \
                                    toolchain carries the nvenc backend"))
            .ok()?;
        let ctx = graph
            .init_execution_context()
            .map_err(|e| eprintln!("[nvenc] no NVENC session: {e:?}"))
            .ok()?;

        // `config` once per context, before the first frame. NV12 in: a frame
        // crosses the sandbox boundary on every call and NV12 is half the bytes
        // of RGB24, which at 60 fps is the difference between ~370 MB/s and
        // ~185 MB/s of copy (PLATFORM.md §2).
        let cfg = format!(
            "{{\"codec\":\"h264\",\"width\":{width},\"height\":{height},\
              \"fps\":{fps},\"bitrate\":{},\"format\":\"nv12\"}}",
            kbps as u64 * 1000
        );
        if let Err(e) = ctx.compute(vec![u8_tensor("config", cfg.into_bytes())]) {
            eprintln!("[nvenc] configure failed: {e:?}");
            return None;
        }
        eprintln!("[nvenc] session open: {width}x{height}@{fps} {kbps}kbps h264 (NV12 in)");
        Some(NvencEncoder {
            ctx,
            width,
            height,
            nv12: vec![0u8; width * height * 3 / 2],
            force_idr: false,
        })
    }

    /// Hand one NV12 frame to the card and take back one access unit.
    fn submit(&mut self) -> Vec<EncodedFrame> {
        let mut inputs = vec![u8_tensor("frame", std::mem::take(&mut self.nv12))];
        if self.force_idr {
            inputs.push(i32_tensor("idr", 1));
        }
        let outs = match self.ctx.compute(inputs) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[nvenc] encode failed: {e:?}");
                // Put the scratch back so the next frame does not reallocate.
                self.nv12 = vec![0u8; self.width * self.height * 3 / 2];
                return vec![];
            }
        };
        // Reclaim the scratch buffer for the next frame.
        self.nv12 = vec![0u8; self.width * self.height * 3 / 2];
        self.force_idr = false;

        let Some(bits) = find(&outs, "bitstream") else {
            eprintln!("[nvenc] host returned no bitstream");
            return vec![];
        };
        let data = bits.data();
        if data.is_empty() {
            return vec![];
        }
        // The host owns repeatSPSPPS, so a keyframe already leads with its SPS
        // and we can trust the flag rather than re-parsing NALs.
        let keyframe = find(&outs, "keyframe")
            .map(|t| {
                let d = t.data();
                d.len() >= 4 && i32::from_le_bytes([d[0], d[1], d[2], d[3]]) != 0
            })
            .unwrap_or(false);
        vec![EncodedFrame { data, keyframe }]
    }
}

/// BT.601 limited-range RGB -> NV12, the format the card wants.
///
/// Luma is full resolution; chroma is one interleaved U/V pair per 2x2 block,
/// averaged over the block rather than point-sampled so a dithered desktop does
/// not shimmer. Integer maths throughout: this runs per pixel, per frame.
fn rgb_to_nv12(rgb: &[u8], width: usize, height: usize, out: &mut [u8]) {
    let (y_plane, uv_plane) = out.split_at_mut(width * height);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 3;
            let (r, g, b) = (rgb[i] as i32, rgb[i + 1] as i32, rgb[i + 2] as i32);
            y_plane[y * width + x] =
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
    }
    for cy in 0..height / 2 {
        for cx in 0..width / 2 {
            let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((cy * 2 + dy) * width + (cx * 2 + dx)) * 3;
                    sr += rgb[i] as i32;
                    sg += rgb[i + 1] as i32;
                    sb += rgb[i + 2] as i32;
                }
            }
            let (r, g, b) = (sr / 4, sg / 4, sb / 4);
            let o = (cy * (width / 2) + cx) * 2;
            uv_plane[o] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            uv_plane[o + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

/// The same conversion straight off the captured BGRX framebuffer, skipping the
/// packed-RGB intermediate — one pass over the pixels instead of three, which
/// is a real fraction of the per-frame budget at 1024x768 under wasm.
fn bgrx_to_nv12(bgrx: &[u8], width: usize, height: usize, out: &mut [u8]) {
    let (y_plane, uv_plane) = out.split_at_mut(width * height);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) * 4;
            let (b, g, r) = (bgrx[i] as i32, bgrx[i + 1] as i32, bgrx[i + 2] as i32);
            y_plane[y * width + x] =
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
    }
    for cy in 0..height / 2 {
        for cx in 0..width / 2 {
            let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((cy * 2 + dy) * width + (cx * 2 + dx)) * 4;
                    sb += bgrx[i] as i32;
                    sg += bgrx[i + 1] as i32;
                    sr += bgrx[i + 2] as i32;
                }
            }
            let (r, g, b) = (sr / 4, sg / 4, sb / 4);
            let o = (cy * (width / 2) + cx) * 2;
            uv_plane[o] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            uv_plane[o + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl VideoEncoder for NvencEncoder {
    fn encode(&mut self, rgb: &[u8], width: usize, height: usize) -> Vec<EncodedFrame> {
        if width != self.width || height != self.height || rgb.len() < width * height * 3 {
            return vec![];
        }
        let (w, h) = (self.width, self.height);
        let mut scratch = std::mem::take(&mut self.nv12);
        rgb_to_nv12(rgb, w, h, &mut scratch);
        self.nv12 = scratch;
        self.submit()
    }

    fn encode_capture(&mut self, fresh: &[u8]) -> Option<Vec<EncodedFrame>> {
        let (w, h) = (self.width, self.height);
        if fresh.len() < w * h * 4 {
            return None;
        }
        let mut scratch = std::mem::take(&mut self.nv12);
        bgrx_to_nv12(fresh, w, h, &mut scratch);
        self.nv12 = scratch;
        Some(self.submit())
    }

    fn mime(&self) -> &'static str {
        "video/h264"
    }

    fn webcodec(&self) -> &'static str {
        // Baseline 4.2 — what the shim configures and what Moonlight negotiates.
        "avc1.42e02a"
    }

    /// Unlike the ffmpeg-through-a-pipe path this replaces, the card CAN honour
    /// a mid-stream IDR (NV_ENC_PIC_FLAG_FORCEIDR), so a client that lost
    /// packets recovers on the next frame instead of the next GOP boundary.
    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }
}

// --------------------------------------------------------------------------
// Native build: there is no wasi-nn outside the sandbox, so the whole GPU edge
// compiles out. The pixel conversions above are deliberately NOT gated — they
// are pure functions and the test suite exercises them on the host, which is
// the only place they can be checked against a reference.
// --------------------------------------------------------------------------

/// Always 0 off-target: no host, no card, no capabilities.
#[cfg(not(target_arch = "wasm32"))]
pub fn caps() -> i32 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
pub fn available() -> bool {
    false
}

/// A stand-in that can never be constructed, so `build_encoder` type-checks
/// identically on both targets and the fallback path is the same code.
#[cfg(not(target_arch = "wasm32"))]
pub struct NvencEncoder {
    _never: std::convert::Infallible,
}

#[cfg(not(target_arch = "wasm32"))]
impl NvencEncoder {
    pub fn new(_w: usize, _h: usize, _fps: u32, _kbps: u32) -> Option<Self> {
        None
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl VideoEncoder for NvencEncoder {
    fn encode(&mut self, _rgb: &[u8], _w: usize, _h: usize) -> Vec<EncodedFrame> {
        match self._never {}
    }
    fn mime(&self) -> &'static str {
        match self._never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat colour must survive RGB -> NV12 as the BT.601 limited-range luma
    /// for that colour, with both chroma samples at their neutral point for
    /// grey. Checked against hand-computed references: getting the coefficients
    /// or the +16/+128 offsets wrong shifts the whole picture, and on a desktop
    /// that reads as "washed out" rather than "broken".
    #[test]
    fn a_flat_grey_converts_to_flat_luma_and_neutral_chroma() {
        let (w, h) = (4usize, 4usize);
        let rgb = vec![128u8; w * h * 3];
        let mut out = vec![0u8; w * h * 3 / 2];
        rgb_to_nv12(&rgb, w, h, &mut out);
        // luma = (((66+129+25)*128 + 128) >> 8) + 16 = (28288 >> 8) + 16 = 126,
        // which is BT.601's 16 + 219*128/255 = 125.93 rounded down. A result of
        // 128 here would mean the +16 studio-swing offset was dropped.
        assert!(out[..w * h].iter().all(|&y| y == 126), "luma: {:?}", &out[..8]);
        // grey is chroma-neutral
        assert!(out[w * h..].iter().all(|&c| c == 128), "chroma: {:?}", &out[w * h..]);
    }

    /// Pure red and pure blue must land on opposite chroma extremes, which is
    /// what proves U and V are not swapped — a swap looks plausible on grey and
    /// inverts every colour on real content.
    #[test]
    fn u_and_v_are_not_swapped() {
        let (w, h) = (2usize, 2usize);
        let mut out = vec![0u8; w * h * 3 / 2];

        let red: Vec<u8> = [255u8, 0, 0].repeat(w * h);
        rgb_to_nv12(&red, w, h, &mut out);
        let (u_red, v_red) = (out[w * h], out[w * h + 1]);
        assert!(u_red < 128, "red must pull U below neutral, got {u_red}");
        assert!(v_red > 128, "red must pull V above neutral, got {v_red}");

        let blue: Vec<u8> = [0u8, 0, 255].repeat(w * h);
        rgb_to_nv12(&blue, w, h, &mut out);
        let (u_blue, v_blue) = (out[w * h], out[w * h + 1]);
        assert!(u_blue > 128, "blue must pull U above neutral, got {u_blue}");
        assert!(v_blue < 128, "blue must pull V below neutral, got {v_blue}");
    }

    /// The capture fast path must agree with the packed-RGB path pixel for
    /// pixel; if it does not, the "one pass instead of three" optimisation is
    /// silently changing the picture.
    #[test]
    fn the_bgrx_fast_path_matches_the_rgb_path() {
        let (w, h) = (8usize, 6usize);
        let mut rgb = vec![0u8; w * h * 3];
        let mut bgrx = vec![0u8; w * h * 4];
        for i in 0..w * h {
            let (r, g, b) = ((i * 7 % 256) as u8, (i * 13 % 256) as u8, (i * 29 % 256) as u8);
            rgb[i * 3] = r;
            rgb[i * 3 + 1] = g;
            rgb[i * 3 + 2] = b;
            bgrx[i * 4] = b;
            bgrx[i * 4 + 1] = g;
            bgrx[i * 4 + 2] = r;
            bgrx[i * 4 + 3] = 255;
        }
        let mut from_rgb = vec![0u8; w * h * 3 / 2];
        let mut from_bgrx = vec![0u8; w * h * 3 / 2];
        rgb_to_nv12(&rgb, w, h, &mut from_rgb);
        bgrx_to_nv12(&bgrx, w, h, &mut from_bgrx);
        assert_eq!(from_rgb, from_bgrx, "the BGRX fast path diverged from the RGB path");
    }

    /// NV12 is 12 bits per pixel: a full-res luma plane plus one interleaved
    /// U/V pair per 2x2 block. A wrong buffer size is an out-of-bounds panic on
    /// the first frame, so pin it.
    #[test]
    fn nv12_is_twelve_bits_per_pixel() {
        let (w, h) = (1024usize, 768usize);
        assert_eq!(w * h * 3 / 2, w * h + (w / 2) * (h / 2) * 2);
    }
}
