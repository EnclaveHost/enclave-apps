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
use std::sync::atomic::{AtomicUsize, Ordering};

pub const FB_BASE: u64 = 0x87e0_0000;
/// The DTB's simple-framebuffer window: 4 MiB reserved, of which a frame may
/// use 3 MiB (1024x768x4, the largest mode).
pub const FB_MAX_BYTES: usize = 0x30_0000;

/// The display's size, fixed for the life of the process by `set_size` before
/// the machine boots (it has to agree with the DTB node the guest kernel reads
/// once, at boot).
///
/// It is a knob because every pixel is paid for three times — the guest draws
/// it, the scan reads it back, the encoder ships it — so a machine running a
/// 320x200 game at 1024x768 spends nine tenths of that work on borders.
static FB_W_CELL: AtomicUsize = AtomicUsize::new(1024);
static FB_H_CELL: AtomicUsize = AtomicUsize::new(768);

pub fn fb_w() -> usize {
    FB_W_CELL.load(Ordering::Relaxed)
}
pub fn fb_h() -> usize {
    FB_H_CELL.load(Ordering::Relaxed)
}
pub fn fb_stride() -> usize {
    fb_w() * 4
}
pub fn fb_bytes() -> usize {
    fb_stride() * fb_h()
}

/// Returns false, changing nothing, for a size that would not fit the reserved
/// window — the same guard the emulator applies to the DTB, so the app and the
/// guest can never disagree about the shape of the screen.
pub fn set_size(w: usize, h: usize) -> bool {
    if w == 0 || h == 0 || w % 2 != 0 || w * h * 4 > FB_MAX_BYTES {
        return false;
    }
    FB_W_CELL.store(w, Ordering::Relaxed);
    FB_H_CELL.store(h, Ordering::Relaxed);
    true
}
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
    scan_interval_boosted(cost, still, false)
}

/// `boosted` halves the floor while input is in flight: a keystroke's pixel
/// is worth scanning for sooner, and the boost window is short enough that
/// the extra captures cost the guest nothing it would notice.
pub fn scan_interval_boosted(cost: std::time::Duration, still: u32, boosted: bool)
    -> std::time::Duration
{
    use std::time::Duration;
    let floor = Duration::from_millis(match boosted {
        true => FB_SCAN_FLOOR_MS / 2,
        false => FB_SCAN_FLOOR_MS,
    });
    let ceiling = Duration::from_millis(FB_SCAN_MS);
    let base = (cost * SCAN_COST_RATIO).clamp(floor, ceiling);
    (base * (1u32 << still.min(SCAN_MAX_BACKOFF))).min(ceiling)
}

pub struct Band {
    /// Left edge and width in pixels. A band used to be a run of WHOLE rows,
    /// which is right for a desktop repainting a strip and wrong for the case
    /// that actually needs the bandwidth: a game in a window, where two thirds
    /// of every "changed" row is a background that did not move. Cropping to
    /// the columns that really changed cuts what the worker deflates and what
    /// the wire carries by the same fraction.
    pub x: usize,
    pub w: usize,
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
    /// Scans since the last full hash. Guest-reported damage is a promise the
    /// host cannot check cheaply, so a full scan runs periodically to repair
    /// anything the promise missed.
    since_full: u32,
}

/// How often a full-frame hash runs even when the guest reported damage.
/// At the 100 ms scan floor this is a repair pass roughly every 6 seconds —
/// often enough that a bad damage rect is a brief artifact, rare enough that
/// the saving is real.
const FULL_SCAN_EVERY: u32 = 60;

/// The game's overlay rectangle (packed x<<48|y<<32|w<<16|h) and when it was
/// last written, in millis since the first capture.
///
/// Module scope rather than function scope because TWO decisions need it: the
/// composite that paints the rectangle, and the arbiter that decides which
/// surface is the live one. The arbiter has to know that framebuffer traffic
/// belongs to the overlay, or the overlay's own writes convince it the dead
/// simple-framebuffer is the screen — which streams a black desktop with the
/// game floating on it (measured: desktop pixels 0,0,0 where fluxbox paints
/// #1a1a2e).
static OVERLAY_RECT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OVERLAY_FRESH_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OVERLAY_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn overlay_now_ms() -> u64 {
    OVERLAY_EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Is a game overlay painting the framebuffer right now?
///
/// Same one-second window the composite uses, so the two agree by
/// construction: while this is true the framebuffer's byte counter is the
/// game's doing and says nothing about which surface the DESKTOP lives on.
fn overlay_is_fresh() -> bool {
    OVERLAY_RECT.load(Ordering::Relaxed) != 0
        && overlay_now_ms().saturating_sub(OVERLAY_FRESH_MS.load(Ordering::Relaxed)) <= 1000
}

impl Display {
    pub fn new() -> Self {
        Display {
            frame: vec![0; fb_bytes()],
            scratch: vec![0; fb_bytes()],
            row_hash: vec![0; fb_h()],
            force_full: true,
            primed: false,
            since_full: 0,
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
    pub fn scan(&mut self, emu: &mut Emulator) -> Vec<Band> {
        // Read into the scratch buffer and swap it in, rather than allocating
        // a fresh frame each time: the row hashes carry everything needed to
        // find what changed, so the previous frame's bytes are not consulted.
        let mut buf = std::mem::take(&mut self.scratch);
        let damage = Self::capture_damage(emu, &mut buf);
        let (bands, spare) = self.bands(buf, damage);
        self.scratch = spare;
        bands
    }

    /// The half of a scan that needs the emulator: copy the framebuffer out of
    /// guest RAM. This is a memcpy and nothing else, which is the point — it
    /// is the only part that cannot leave the emulator's thread, so it is kept
    /// as small as possible. Everything expensive (hashing, diffing,
    /// deflating) is in `bands` and can run anywhere.
    pub fn capture(emu: &Emulator, out: &mut Vec<u8>) {
        // Track the guest's chosen mode BEFORE sizing the buffer: with
        // virtio-gpu the resolution is the guest's to change at runtime, so
        // the DTB's numbers are a starting point rather than the truth.
        if let Some((w, h)) = emu.gpu_mode() {
            if w as usize != fb_w() || h as usize != fb_h() {
                // set_size refuses a mode that would not fit the reserved
                // window; a refusal leaves the old geometry standing and
                // read_display falls back rather than blitting a mismatch.
                set_size(w as usize, h as usize);
            }
        }
        out.resize(fb_bytes(), 0);
        let from_gpu = emu.read_display(FB_BASE, out, Self::gpu_is_the_live_surface(emu));
        if from_gpu {
            Self::composite_overlay(emu, out);
        }
    }

    /// Host-side game overlay: on the virtio-gpu image the simple-framebuffer
    /// is invisible dead memory, so a game writing it via xdoom's -overlay
    /// path pays ONE blit per frame and the desktop's whole X copy chain
    /// (SHM -> shadow -> scanout buffer) disappears from the guest's budget.
    /// The composition happens HERE, natively: whatever rectangle the guest
    /// wrote into the fb window recently is copied over the GPU scanout at
    /// capture time. Freshness rides the write stream — a game that stops
    /// painting stops being composited after a second, and X's own (stale)
    /// window contents show, which is the same frame the game last drew.
    fn composite_overlay(emu: &Emulator, out: &mut [u8]) {
        let now_ms = overlay_now_ms();
        if let Some((x, y, w, h)) = emu.fb_take_overlay_rect() {
            // ignore boot-console noise: the overlay is a WINDOW, not the
            // whole screen; full-height rects are fbcon, not the game
            if (h as usize) < fb_h() && w > 0 && h > 0 {
                // Track the game window, and let it MOVE. A frame's fb0 write is
                // often a sub-region (the HUD, a few changed rows), so shrinking
                // the box to it would leave the rest of the window showing the
                // stale GPU scanout underneath. But a plain grow-forever union
                // was worse: dragging the window kept the OLD position in the
                // box, so the game's stale pixels stayed painted there over the
                // desktop — "pixels in front of the window" after a move.
                //
                // The out is that a move forces a full-window repaint (the WM's
                // expose), so a BIG damage rect is the signal to snap the box to
                // the current position; a small one just refreshes in place.
                // Replace the box when this frame redrew most of it (a full
                // redraw or a move), keep it for a sub-region update.
                let stale = now_ms
                    .saturating_sub(OVERLAY_FRESH_MS.load(Ordering::Relaxed))
                    > 1000;
                let prev = OVERLAY_RECT.load(Ordering::Relaxed);
                let (dx, dy, dw, dh) = (x as u32, y as u32, w as u32, h as u32);
                let (nx, ny, nw, nh) = if prev == 0 || stale {
                    (dx, dy, dw, dh)
                } else {
                    let pw = (prev >> 16 & 0xffff) as u32;
                    let ph = (prev & 0xffff) as u32;
                    let box_area = (pw as u64) * (ph as u64);
                    let dmg_area = (dw as u64) * (dh as u64);
                    // A repaint covering half the box or more is a full redraw
                    // or a move: snap to it. Otherwise keep the box and refresh
                    // its live pixels in place.
                    if dmg_area * 2 >= box_area {
                        (dx, dy, dw, dh)
                    } else {
                        let px = (prev >> 48 & 0xffff) as u32;
                        let py = (prev >> 32 & 0xffff) as u32;
                        (px, py, pw, ph)
                    }
                };
                OVERLAY_RECT.store(
                    (nx as u64) << 48 | (ny as u64) << 32 | (nw as u64) << 16 | nh as u64,
                    Ordering::Relaxed,
                );
                OVERLAY_FRESH_MS.store(now_ms, Ordering::Relaxed);
            }
        }
        let packed = OVERLAY_RECT.load(Ordering::Relaxed);
        if packed == 0 || now_ms.saturating_sub(OVERLAY_FRESH_MS.load(Ordering::Relaxed)) > 1000 {
            return;
        }
        let (x, y, w, h) = (
            (packed >> 48 & 0xffff) as usize,
            (packed >> 32 & 0xffff) as usize,
            (packed >> 16 & 0xffff) as usize,
            (packed & 0xffff) as usize,
        );
        let stride = fb_stride();
        let (sw, sh) = (fb_w(), fb_h());
        if x >= sw || y >= sh {
            return;
        }
        let w = w.min(sw - x);
        let h = h.min(sh - y);
        let mut row = vec![0u8; w * 4];
        for r in 0..h {
            let off = (y + r) * stride + x * 4;
            emu.read_physical_range(FB_BASE + off as u64, &mut row);
            out[off..off + w * 4].copy_from_slice(&row);
        }
    }

    /// Which display device is the guest actually DRAWING to?
    ///
    /// Existence is not use, and getting this wrong streams a blank screen.
    /// The kernel's fbdev emulation binds a virtio-gpu scanout at boot and
    /// flushes it once, so "a scanout is bound" is true from early boot even
    /// while the whole desktop paints the simple-framebuffer. Both surfaces
    /// carry a monotonic activity counter — framebuffer bytes written, scanout
    /// flushes — so follow whichever MOVED since the last frame, and stay put
    /// when neither did (a still screen must not flip the source).
    fn gpu_is_the_live_surface(emu: &Emulator) -> bool {
        use std::sync::atomic::AtomicBool;
        static GPU_SEEN: AtomicBool = AtomicBool::new(false);

        // Once a virtio-gpu scanout has ever been flushed, this machine has a
        // GPU desktop, and the GPU scanout is the base FOR GOOD — the game is
        // composited on top of it (composite_overlay), never shown instead of
        // it. The previous "whichever surface moved most recently" arbiter
        // flip-flopped on exactly the frames both moved, and each symptom is
        // one side of that flip:
        //
        //   * A running game paints the simple-framebuffer every frame while a
        //     still desktop paints nothing, so the arbiter read fb0 alone and
        //     streamed the game on a black screen until an interaction forced a
        //     GPU repaint. ("Desktop is black until I click.")
        //   * During a menu transition both surfaces move, so it alternated
        //     GPU-with-overlay / fb0-alone frame by frame — the desktop present
        //     on one frame and gone the next. ("Flashes the frame from right
        //     before, every other frame.")
        //
        // The fb-only DOOM machine has no virtio-gpu, never flushes one, so
        // GPU_SEEN stays false and it keeps reading the simple-framebuffer.
        if emu.gpu_flushes() > 0 {
            GPU_SEEN.store(true, Ordering::Relaxed);
        }
        GPU_SEEN.load(Ordering::Relaxed)
    }

    /// Capture, and take the guest's damage report with it. One call so the
    /// two cannot drift: taking the region CLEARS it, so a caller that
    /// captured without taking would scan a frame whose damage had already
    /// been consumed by someone else.
    pub fn capture_damage(emu: &mut Emulator, out: &mut Vec<u8>) -> Option<(usize, usize)> {
        Self::capture(emu, out);
        // Only the row range matters: the column range is recovered by
        // comparing the rows we do hash, and that comparison is cheap next to
        // the hashing this avoids.
        emu.gpu_take_dirty().map(|(_, y, _, h)| {
            let y0 = y as usize;
            (y0.min(fb_h()), (y0 + h as usize).min(fb_h()))
        })
    }

    /// Turn a captured frame into bands. Takes the frame by value and hands
    /// back the frame it replaced, so a caller holding a buffer pool can keep
    /// recycling two buffers forever instead of allocating megabytes per scan.
    /// `damage` is the row range the GUEST said it changed (virtio-gpu's
    /// RESOURCE_FLUSH). Rows outside it are not hashed at all, which is the
    /// entire point of having a display controller: finding what moved costs
    /// a 3 MB hash of the whole frame otherwise, every scan, forever.
    ///
    /// Damage is TRUSTED BUT VERIFIED. A rectangle that is wrong or
    /// incomplete would freeze part of the picture indefinitely, so every
    /// FULL_SCAN_EVERY scans one full hash runs regardless and repairs
    /// whatever the guest failed to mention. That bounds a bad rect to well
    /// under a second instead of forever.
    pub fn bands(&mut self, mut frame: Vec<u8>, damage: Option<(usize, usize)>)
        -> (Vec<Band>, Vec<u8>) {
        let mut dirty = vec![false; fb_h()];
        let mut any = false;
        self.since_full = self.since_full.wrapping_add(1);
        let heal = self.since_full >= FULL_SCAN_EVERY;
        if heal {
            self.since_full = 0;
        }
        let (scan_y0, scan_y1) = match damage {
            Some((y0, y1)) if self.primed && !heal && !self.force_full => {
                (y0.min(fb_h()), y1.min(fb_h()))
            }
            _ => (0, fb_h()),
        };
        // Columns that changed anywhere on the screen, in 8-byte units (two
        // pixels). Found while the OLD frame is still in place, because a
        // column range needs the two frames compared, not just their hashes —
        // and it is what lets a band be a rectangle instead of a full-width
        // strip. Two u64 compares per changed pair of pixels is cheap next to
        // deflating the columns that did not change.
        let stride = fb_stride();
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for y in scan_y0..scan_y1 {
            let row = &frame[y * stride..(y + 1) * stride];
            let h = fnv1a(row);
            if h == self.row_hash[y] && self.primed {
                continue;
            }
            self.row_hash[y] = h;
            dirty[y] = true;
            any = true;
            if !self.primed {
                continue; // no previous frame to compare against
            }
            let old = &self.frame[y * stride..(y + 1) * stride];
            let mut i = 0usize;
            while i < stride {
                if row[i..i + 8] != old[i..i + 8] {
                    if i < lo {
                        lo = i;
                    }
                    if i + 8 > hi {
                        hi = i + 8;
                    }
                }
                i += 8;
            }
        }
        let full = self.force_full;
        self.force_full = false;
        let was_primed = self.primed;
        self.primed = true;
        // `frame` becomes the current one; the old current goes back to the
        // caller as the next capture target.
        std::mem::swap(&mut self.frame, &mut frame);
        if full {
            return (vec![self.band(0, fb_w(), 0, fb_h())], frame);
        }
        if !any {
            return (Vec::new(), frame);
        }
        // A dirty row with no differing column pair cannot happen (the hash
        // changed), but a hash collision or an unprimed first scan can leave
        // the range unset — fall back to the whole width rather than crop
        // wrongly.
        let (x, w) = match was_primed && lo != usize::MAX && hi > lo {
            true => (lo / 4, (hi - lo) / 4),
            false => (0, fb_w()),
        };
        (self.group_dirty(&dirty, x, w), frame)
    }

    fn group_dirty(&self, dirty: &[bool], x: usize, w: usize) -> Vec<Band> {
        // group consecutive dirty rows; sew gaps under 8 rows into one band
        // (fewer events beats a few clean rows re-sent inside a run)
        let mut bands = Vec::new();
        let mut y = 0usize;
        while y < fb_h() {
            if !dirty[y] {
                y += 1;
                continue;
            }
            let start = y;
            let mut end = y + 1; // exclusive
            let mut gap = 0usize;
            let mut z = end;
            while z < fb_h() && gap < 8 {
                if dirty[z] {
                    end = z + 1;
                    gap = 0;
                } else {
                    gap += 1;
                }
                z += 1;
            }
            bands.push(self.band(x, w, start, end - start));
            y = end + gap;
        }
        bands
    }

    fn band(&self, x: usize, w: usize, y: usize, h: usize) -> Band {
        let stride = fb_stride();
        // A full-width band is already contiguous; a cropped one is gathered
        // into a scratch buffer so the compressor still sees one run of bytes.
        let cropped;
        let rows: &[u8] = match w == fb_w() {
            true => &self.frame[y * stride..(y + h) * stride],
            false => {
                let mut out = Vec::with_capacity(w * 4 * h);
                for row in y..y + h {
                    let off = row * stride + x * 4;
                    out.extend_from_slice(&self.frame[off..off + w * 4]);
                }
                cropped = out;
                &cropped
            }
        };
        // A big band is a moving picture, and there the limit on what a watcher
        // sees is how fast this can be produced, not how fast it can be sent:
        // one frame is in flight at a time, so the deflate time IS the frame
        // interval. Level 1 runs about three times faster for about a quarter
        // more bytes, which is the right trade at 30 frames a second and the
        // wrong one for a text screen changing a single line — where the bytes
        // are few, the time is nothing, and level 6 compresses text far better.
        let level = match rows.len() > 256 * 1024 {
            true => 1,
            false => 6,
        };
        Band { x, y, w, h, z: miniz_oxide::deflate::compress_to_vec(rows, level) }
    }

    /// The current frame as a PNG (fresh scan first so a GET with no SSE
    /// watcher still sees live pixels).
    pub fn png(&mut self, emu: &Emulator) -> Vec<u8> {
        let mut fresh = vec![0u8; fb_bytes()];
        let prefer = Self::gpu_is_the_live_surface(emu);
        // Composite the game overlay too, so a snapshot shows exactly what the
        // video stream shows — otherwise /fb.png reads the GPU scanout without
        // the live game on top and diverges from what a viewer sees.
        if emu.read_display(FB_BASE, &mut fresh, prefer) {
            Self::composite_overlay(emu, &mut fresh);
        }
        // raw scanlines: filter byte 0 + RGB (drop X, reorder BGR -> RGB)
        let mut raw = Vec::with_capacity(fb_h() * (1 + fb_w() * 3));
        for y in 0..fb_h() {
            raw.push(0u8);
            let row = &fresh[y * fb_stride()..(y + 1) * fb_stride()];
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
        ihdr.extend_from_slice(&(fb_w() as u32).to_be_bytes());
        ihdr.extend_from_slice(&(fb_h() as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolor, deflate, filter 0, no interlace
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &idat);
        chunk(&mut png, b"IEND", &[]);
        png
    }
}

/// FNV-1a, eight bytes at a time.
///
/// Not the standard byte-wise mixing — this is a row-change detector, not a
/// hash anyone else consumes, and a row is 4 KiB. Byte-wise it was 3 million
/// multiply-xor rounds per 1024x768 scan, which on the display worker's core
/// cost more than deflating the bands did. Reading `u64` at a time keeps the
/// avalanche (every input byte still reaches the accumulator through the
/// multiply) at an eighth of the rounds; rows are 4-byte pixels so the tail is
/// at most seven bytes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let (chunks, tail) = bytes.split_at(bytes.len() & !7);
    for c in chunks.chunks_exact(8) {
        h ^= u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    for &b in tail {
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
