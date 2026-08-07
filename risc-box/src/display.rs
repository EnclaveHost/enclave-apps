//! The machine's display, scanned out of guest RAM to the browser.
//!
//! The emulator's default device tree declares a `simple-framebuffer`:
//! 1024x768, 32-bit XRGB, in a 4 MiB reserved window of the guest DRAM
//! (0x87e00000). A guest kernel with CONFIG_FB_SIMPLE drives it as /dev/fb0
//! (fbcon, fbdev Xorg, Wayland via wlroots' fbdev — anything); a kernel
//! without it ignores the node and the serial console remains the only view.
//! There is no device model at all on the emulator side: the "card" is plain
//! RAM the kernel is told about, and this module is the monitor cable.
//!
//! Scanout: while at least one browser watches (SSE topic "display"), the
//! frame is read out of guest physical memory and diffed
//! row-wise against the previous scan (FNV-1a per row). Runs of changed rows
//! become BANDS; each band ships as one SSE event of raw-deflate bytes
//! (base64) that the browser inflates with DecompressionStream("deflate-raw")
//! and blits with putImageData. A newly joined watcher forces one full-frame
//! band so it starts from truth, not from deltas.
//!
//! /fb.png renders the CURRENT frame as a PNG (truecolor, filter 0, zlib
//! IDAT) — one attested pixel-exact snapshot per GET, which is also what the
//! end-to-end verification diffs to prove "the cube spins".
//!
//! Pixel format note: x8r8g8b8 little-endian means bytes B,G,R,X per pixel.
//! Both consumers reorder to RGB(A) themselves; the wire stays the guest's
//! byte view (deflate loves the stable layout, and the app never touches
//! per-pixel work outside a dirty band).

use riscv_emu_rust::Emulator;

pub const FB_BASE: u64 = 0x87e0_0000;
pub const FB_W: usize = 1024;
pub const FB_H: usize = 768;
pub const FB_STRIDE: usize = FB_W * 4;
pub const FB_BYTES: usize = FB_STRIDE * FB_H;
/// Slowest the scan is allowed to get, and the floor the AV1 path paces
/// against. Also the cadence a scan falls back to when it is expensive.
pub const FB_SCAN_MS: u64 = 100;
/// Fastest the scan is allowed to get. 60 fps of diffs is already more than a
/// ~30 MIPS guest can redraw, so scanning harder than this only spends guest
/// time to re-send pixels nobody changed.
pub const FB_SCAN_FLOOR_MS: u64 = 16;

/// Fraction of the emulator thread a scan may take: at most 1/(1+RATIO).
const SCAN_COST_RATIO: u32 = 4;
/// Doublings of the interval allowed while the picture is not moving.
const SCAN_MAX_BACKOFF: u32 = 3;

/// How long to wait before the next scan, given what the last one cost and how
/// many scans in a row have found nothing.
///
/// Two pressures, pulling opposite ways. Scanning costs the guest directly —
/// it is the same thread — so an expensive scan has to be followed by a longer
/// gap or watching the machine slows the machine. But a still screen makes
/// scans CHEAP, and a pure cost budget would then scan flat out to keep
/// finding nothing. Hence the backoff: cost sets the fast rate, stillness
/// decides whether we are entitled to it.
pub fn scan_interval(cost: std::time::Duration, still: u32) -> std::time::Duration {
    use std::time::Duration;
    let floor = Duration::from_millis(FB_SCAN_FLOOR_MS);
    let ceiling = Duration::from_millis(FB_SCAN_MS);
    let base = (cost * SCAN_COST_RATIO).clamp(floor, ceiling);
    (base * (1u32 << still.min(SCAN_MAX_BACKOFF))).min(ceiling)
}

pub struct Band {
    pub y: usize,
    pub h: usize,
    /// raw-deflate of the band's rows, still B,G,R,X
    pub z: Vec<u8>,
}

pub struct Display {
    frame: Vec<u8>,        // last scanned frame (guest byte order)
    // Scratch the next frame is read into, then swapped with `frame`. It only
    // exists so a scan allocates nothing: this buffer is megabytes, a scan
    // happens ten times a second, and the allocation, the zeroing Vec does on
    // the way in and the free on the way out are all pure overhead charged to
    // the same thread that runs the guest.
    scratch: Vec<u8>,
    row_hash: Vec<u64>,    // FNV-1a per row of `frame`
    force_full: bool,      // a watcher joined: next scan ships the whole frame
    primed: bool,          // false until the first scan after boot
}

impl Display {
    pub fn new() -> Self {
        Display {
            frame: vec![0; FB_BYTES],
            scratch: vec![0; FB_BYTES],
            row_hash: vec![0; FB_H],
            force_full: true,
            primed: false,
        }
    }

    /// Forget everything scanned so far (machine stopped or rebooted): the
    /// next watched scan re-primes and ships a full frame.
    pub fn reset(&mut self) {
        self.frame.iter_mut().for_each(|b| *b = 0);
        self.row_hash.iter_mut().for_each(|h| *h = 0);
        self.force_full = true;
        self.primed = false;
    }

    /// A new SSE watcher arrived: make the next scan a full-frame band.
    pub fn want_full(&mut self) {
        self.force_full = true;
    }

    /// Scan the guest framebuffer and return the changed bands (possibly one
    /// full-frame band). Empty when nothing changed.
    pub fn scan(&mut self, emu: &Emulator) -> Vec<Band> {
        // Read into the scratch buffer and swap it in, rather than allocating
        // a fresh frame each time: the row hashes carry everything needed to
        // find what changed, so the previous frame's bytes are not consulted.
        emu.read_physical_range(FB_BASE, &mut self.scratch);
        let mut dirty = vec![false; FB_H];
        let mut any = false;
        for y in 0..FB_H {
            let h = fnv1a(&self.scratch[y * FB_STRIDE..(y + 1) * FB_STRIDE]);
            if h != self.row_hash[y] || !self.primed {
                self.row_hash[y] = h;
                dirty[y] = true;
                any = true;
            }
        }
        let full = self.force_full;
        self.force_full = false;
        self.primed = true;
        std::mem::swap(&mut self.frame, &mut self.scratch);
        if full {
            return vec![self.band(0, FB_H)];
        }
        if !any {
            return Vec::new();
        }
        // group consecutive dirty rows; sew gaps under 8 rows into one band
        // (fewer events beats a few clean rows re-sent inside a run)
        let mut bands = Vec::new();
        let mut y = 0usize;
        while y < FB_H {
            if !dirty[y] {
                y += 1;
                continue;
            }
            let start = y;
            let mut end = y + 1; // exclusive
            let mut gap = 0usize;
            let mut z = end;
            while z < FB_H && gap < 8 {
                if dirty[z] {
                    end = z + 1;
                    gap = 0;
                } else {
                    gap += 1;
                }
                z += 1;
            }
            bands.push(self.band(start, end - start));
            y = end + gap;
        }
        bands
    }

    fn band(&self, y: usize, h: usize) -> Band {
        let rows = &self.frame[y * FB_STRIDE..(y + h) * FB_STRIDE];
        Band { y, h, z: miniz_oxide::deflate::compress_to_vec(rows, 6) }
    }

    /// The current frame as a PNG (fresh scan first so a GET with no SSE
    /// watcher still sees live pixels).
    pub fn png(&mut self, emu: &Emulator) -> Vec<u8> {
        let mut fresh = vec![0u8; FB_BYTES];
        emu.read_physical_range(FB_BASE, &mut fresh);
        // raw scanlines: filter byte 0 + RGB (drop X, reorder BGR -> RGB)
        let mut raw = Vec::with_capacity(FB_H * (1 + FB_W * 3));
        for y in 0..FB_H {
            raw.push(0u8);
            let row = &fresh[y * FB_STRIDE..(y + 1) * FB_STRIDE];
            for px in row.chunks_exact(4) {
                raw.push(px[2]); // R
                raw.push(px[1]); // G
                raw.push(px[0]); // B
            }
        }
        let idat = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
        let mut png = Vec::with_capacity(idat.len() + 64);
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&(FB_W as u32).to_be_bytes());
        ihdr.extend_from_slice(&(FB_H as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor, deflate, filter 0, no interlace
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &idat);
        chunk(&mut png, b"IEND", &[]);
        png
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

struct Crc32 {
    table: [u32; 256],
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        let mut table = [0u32; 256];
        for n in 0..256u32 {
            let mut c = n;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            table[n as usize] = c;
        }
        Crc32 { table, value: 0xffff_ffff }
    }
    fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.value = self.table[((self.value ^ b as u32) & 0xff) as usize] ^ (self.value >> 8);
        }
    }
    fn finish(&self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The whole point of pacing by cost is that an expensive scan buys itself
    /// a longer gap. Pin the budget: a scan may never take more than a fifth
    /// of the thread, however cheap it looks.
    #[test]
    fn an_expensive_scan_earns_a_longer_gap() {
        let cheap = scan_interval(Duration::from_millis(1), 0);
        let dear = scan_interval(Duration::from_millis(15), 0);
        assert!(dear > cheap, "a costlier scan must wait longer, got {dear:?} vs {cheap:?}");

        for ms in [1u64, 4, 6, 15, 40] {
            let cost = Duration::from_millis(ms);
            let gap = scan_interval(cost, 0);
            // Unless we are pinned at the floor, the gap covers RATIO times
            // the work, so scanning stays under 1/(1+RATIO) of the thread.
            if gap > Duration::from_millis(FB_SCAN_FLOOR_MS) && gap < Duration::from_millis(FB_SCAN_MS) {
                assert!(gap >= cost * 4, "cost {cost:?} only earned {gap:?}");
            }
        }
    }

    /// A still picture must not be more expensive to watch than it was under
    /// the old fixed clock: backing off has to reach FB_SCAN_MS.
    #[test]
    fn a_still_screen_backs_off_to_the_old_cadence() {
        let cheap = Duration::from_millis(1);
        assert_eq!(scan_interval(cheap, 0), Duration::from_millis(FB_SCAN_FLOOR_MS));

        let settled = scan_interval(cheap, 10);
        assert_eq!(
            settled,
            Duration::from_millis(FB_SCAN_MS),
            "a screen that has been still for a while should cost no more than it used to"
        );

        // Monotonic on the way there, so the cost falls off smoothly.
        let mut previous = Duration::ZERO;
        for still in 0..12 {
            let gap = scan_interval(cheap, still);
            assert!(gap >= previous, "backoff went backwards at {still}");
            previous = gap;
        }
    }

    /// ...and the moment something moves, the fast rate is available again.
    #[test]
    fn motion_snaps_back_to_the_fast_rate() {
        let cheap = Duration::from_millis(1);
        assert_eq!(scan_interval(cheap, 5), Duration::from_millis(FB_SCAN_MS));
        assert_eq!(scan_interval(cheap, 0), Duration::from_millis(FB_SCAN_FLOOR_MS));
    }

    /// The interval is always inside the declared bounds, whatever it is fed.
    #[test]
    fn the_interval_stays_within_its_bounds() {
        for ms in [0u64, 1, 7, 50, 250, 5_000] {
            for still in [0u32, 1, 3, 7, u32::MAX] {
                let gap = scan_interval(Duration::from_millis(ms), still);
                assert!(
                    gap >= Duration::from_millis(FB_SCAN_FLOOR_MS)
                        && gap <= Duration::from_millis(FB_SCAN_MS),
                    "cost {ms}ms still {still} produced {gap:?}"
                );
            }
        }
    }
}
