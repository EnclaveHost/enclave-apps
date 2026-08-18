//! A local mirror of the guest's framebuffer, fed by the app's `/display`
//! stream, so the encoder can read frames without fetching them.
//!
//! The obvious frame source is `GET /fb.rgb`, and for a bridge sitting next to
//! the app it is the right one: a whole framebuffer, no state, no protocol.
//! Over a network it stops working. The frame is 1024*768*3 = 2.25 MiB and a
//! measured fetch from a deployment on the fleet took **2.9 seconds** — about
//! a third of a frame per second, where the encoder wants sixty.
//!
//! `/display` is the same picture at a fraction of the bytes. The app already
//! scans its framebuffer, finds the rows that changed, and ships them
//! deflate-compressed as SSE events; that is the path the browser uses, and it
//! costs the guest a fifth of its thread at most, against AV1's same budget
//! for far more work per frame. So: hold one long-lived connection,
//! apply each band to a locally-held copy, and let the encoder read that copy
//! as fast as it likes. Bandwidth becomes proportional to what MOVED rather
//! than to frame rate, which for a desktop that is mostly still is close to
//! nothing.
//!
//! Byte order is the one wrinkle. Bands carry the guest's framebuffer bytes
//! verbatim — B,G,R,X per pixel — while the encoder is fed rgb24. The
//! conversion happens once here, on the way in.

use std::io::BufRead;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::app::App;

/// Bytes per pixel in a `/display` band (the guest's own B,G,R,X layout).
const BAND_BPP: usize = 4;
/// The client-side pointer. The guest's X server runs -nocursor (an in-frame
/// arrow could only ever trail the real pointer by a full round trip), so the
/// bridge composites its own sprite at the position it last FORWARDED — the
/// cursor moves with zero perceived latency and the screen underneath catches
/// up. Packed (x<<32)|y plus a "shown" bit; updated by the control channel.
static CURSOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// Position is NORMALIZED (0..1 of the frame), stored as micro-units; the
/// compositor scales by its own frame size, so the control channel needs no
/// knowledge of the screen geometry.
pub fn cursor_set(fx: f64, fy: f64) {
    let x = (fx.clamp(0.0, 1.0) * 1_000_000.0) as u64;
    let y = (fy.clamp(0.0, 1.0) * 1_000_000.0) as u64;
    CURSOR.store((x << 32) | y, std::sync::atomic::Ordering::Relaxed);
    // A moved pointer is a changed picture even when no band arrived: nudge
    // every snapshot_if_changed consumer to re-encode.
    CURSOR_MOVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

static CURSOR_MOVES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn cursor_get() -> Option<(usize, usize)> {
    let v = CURSOR.load(std::sync::atomic::Ordering::Relaxed);
    match v == u64::MAX {
        true => None,
        false => Some(((v >> 32) as usize, (v & 0xffff_ffff) as usize)),
    }
}

/// A tiny left-arrow: 1 = black border, 2 = white fill, 0 = transparent.
const CURSOR_SPRITE: [[u8; 8]; 12] = [
    [1,0,0,0,0,0,0,0],
    [1,1,0,0,0,0,0,0],
    [1,2,1,0,0,0,0,0],
    [1,2,2,1,0,0,0,0],
    [1,2,2,2,1,0,0,0],
    [1,2,2,2,2,1,0,0],
    [1,2,2,2,2,2,1,0],
    [1,2,2,2,2,2,2,1],
    [1,2,2,1,1,1,1,1],
    [1,2,1,0,0,0,0,0],
    [1,1,0,0,0,0,0,0],
    [1,0,0,0,0,0,0,0],
];

/// Bytes per pixel the encoder is fed.
const RGB_BPP: usize = 3;

pub struct Screen {
    frame: Mutex<Vec<u8>>,
    /// Bumped on every applied band, so a reader can tell "nothing has changed
    /// since last time" from "the screen is genuinely still".
    generation: AtomicU64,
    width: usize,
    height: usize,
}

impl Screen {
    /// Start mirroring. Spawns a thread that holds the `/display` stream open
    /// and reconnects if it drops; the screen simply stops updating in between,
    /// which shows as a frozen picture rather than a dead stream.
    pub fn start(app: Arc<App>, width: usize, height: usize) -> Arc<Screen> {
        let screen = Arc::new(Screen {
            frame: Mutex::new(vec![0u8; width * height * RGB_BPP]),
            generation: AtomicU64::new(0),
            width,
            height,
        });
        let worker = screen.clone();
        std::thread::spawn(move || loop {
            match worker.run_once(&app) {
                Ok(()) => eprintln!("[screen] /display stream ended; reconnecting"),
                Err(e) => eprintln!("[screen] /display stream failed ({e}); reconnecting"),
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        screen
    }

    /// Like `start`, but pull-paced (see `run_pull`). Needs an app serving
    /// GET /fb.bands (risc-box 0.6.29+ / risc-box-doom 0.7.7+).
    pub fn start_pull(app: Arc<App>, width: usize, height: usize) -> Arc<Screen> {
        let screen = Arc::new(Screen {
            frame: Mutex::new(vec![0u8; width * height * RGB_BPP]),
            generation: AtomicU64::new(0),
            width,
            height,
        });
        let worker = screen.clone();
        std::thread::spawn(move || loop {
            match worker.run_pull(&app) {
                Ok(()) => eprintln!("[screen] /fb.bands pull ended; redialing"),
                Err(e) => eprintln!("[screen] /fb.bands pull failed ({e}); redialing"),
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        screen
    }

    /// Copy the current picture out, in rgb24. Returns the generation so the
    /// caller can skip re-encoding an unchanged screen if it wants to.
    fn composite_cursor(&self, out: &mut [u8]) {
        let Some((ux, uy)) = cursor_get() else { return };
        let cx = ux * self.width / 1_000_000;
        let cy = uy * self.height / 1_000_000;
        for (dy, row) in CURSOR_SPRITE.iter().enumerate() {
            let y = cy + dy;
            if y >= self.height { break; }
            for (dx, &c) in row.iter().enumerate() {
                if c == 0 { continue; }
                let x = cx + dx;
                if x >= self.width { break; }
                let o = (y * self.width + x) * RGB_BPP;
                let v = if c == 1 { 0u8 } else { 255u8 };
                out[o] = v; out[o + 1] = v; out[o + 2] = v;
            }
        }
    }

    pub fn snapshot_into(&self, out: &mut Vec<u8>) -> u64 {
        let g = {
            let f = self.frame.lock().unwrap();
            out.clear();
            out.extend_from_slice(&f);
            self.generation.load(Ordering::Relaxed)
        };
        self.composite_cursor(out);
        g
    }

    /// Same, but skip the copy entirely when nothing has changed since
    /// `since`. Returns the current generation, so `== since` means `out` was
    /// left alone and still holds a good picture.
    ///
    /// Worth the extra entry point: the encoder asks for a frame at the
    /// negotiated rate (60/s), the mirror changes far less often than that,
    /// and the copy is 2.25 MiB held under the same lock `apply` needs. Doing
    /// it unconditionally does not just waste memory bandwidth — it stands in
    /// the way of the bands coming in off the network.
    pub fn snapshot_if_changed(&self, out: &mut Vec<u8>, since: u64) -> u64 {
        // Checked before taking the lock: the common case is "unchanged", and
        // that case should not touch the mutex at all. Cursor motion counts
        // as change: the composited pointer is part of the picture.
        let now = self.generation.load(Ordering::Acquire)
            .wrapping_add(CURSOR_MOVES.load(Ordering::Relaxed) << 20);
        if now == since && !out.is_empty() {
            return now;
        }
        let g = {
            let f = self.frame.lock().unwrap();
            out.clear();
            out.extend_from_slice(&f);
            self.generation.load(Ordering::Relaxed)
        };
        self.composite_cursor(out);
        g
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn run_once(&self, app: &App) -> std::io::Result<()> {
        let mut r = app.get_stream("/display")?;
        eprintln!("[screen] mirroring {}x{} from /display", self.width, self.height);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let Some(payload) = line.strip_prefix("data: ") else { continue };
            let payload = payload.trim_end();
            // The first frame is `event: mode` with the geometry; the rest are
            // bands. Anything without a "b" field is not a band.
            self.apply_band_payload(payload);
        }
    }

    /// One band, as its JSON object (the SSE `data:` payload and each
    /// /fb.bands event are the same shape). Not-a-band payloads are ignored.
    fn apply_band_payload(&self, payload: &str) {
        {
            let Some(b64) = json_str(payload, "b") else { return };
            let (Some(y), Some(h)) = (json_num(payload, "y"), json_num(payload, "h")) else {
                return;
            };
            // A band used to be a run of whole rows; it is now a rectangle, and
            // carries only the columns that changed. Absent x/w means an older
            // app on the other end, where full width was the only shape there
            // was — so that is what they default to.
            let x = json_num(payload, "x").unwrap_or(0);
            let w = json_num(payload, "w").unwrap_or(self.width);
            let Some(deflated) = b64_decode(b64) else {
                eprintln!("[screen] band with undecodable base64, skipped");
                return;
            };
            let Ok(rows) = miniz_oxide::inflate::decompress_to_vec(&deflated) else {
                eprintln!("[screen] band failed to inflate, skipped");
                return;
            };
            if std::env::var_os("GS_SCREEN_TRACE").is_some() {
                eprintln!(
                    "[screen] band x={x} w={w} y={y} h={h} b64={} inflated={}",
                    b64.len(),
                    rows.len()
                );
            }
            self.apply(x, w, y, h, &rows);
        }
    }

    /// The pull-paced mirror: ask /fb.bands for whatever changed since the
    /// last reply, apply it, ask again. In-flight data is never more than one
    /// response, so the mirror's lag is this link's latency — not the
    /// megabytes of relay buffering the pushed stream accumulates whenever
    /// the screen changes faster than the link can carry (which is what put
    /// a driven cursor seconds behind a perfectly smooth picture). When the
    /// screen outruns the link the app re-bases us with a fresh full frame:
    /// fewer, current pictures instead of every stale delta.
    fn run_pull(&self, app: &App) -> std::io::Result<()> {
        eprintln!("[screen] pull-mirroring {}x{} from /fb.bands", self.width, self.height);
        let mut since: usize = 0;
        loop {
            let asked = std::time::Instant::now();
            let body = app.get(&format!("/fb.bands?since={since}&wait=1"))?;
            let body = String::from_utf8_lossy(&body);
            let gen = json_num(&body, "gen").unwrap_or(since);
            let empty = match body.find("\"events\":[") {
                Some(i) => {
                    let evs = &body[i + 10..body.rfind(']').map(|j| j.max(i + 10)).unwrap_or(i + 10)];
                    for part in evs.split("},{") {
                        self.apply_band_payload(part);
                    }
                    evs.trim().is_empty()
                }
                None => true,
            };
            since = gen;
            // wait=1 long-polls: the server paces us (a band releases the
            // request instantly; silence answers empty at ~150ms), so an
            // empty reply normally just re-parks. An app that predates the
            // wait parameter ignores it and answers empty IMMEDIATELY —
            // detectable as an empty reply faster than any park could be —
            // and without this sleep that would be a hot request loop.
            if empty && asked.elapsed() < std::time::Duration::from_millis(60) {
                std::thread::sleep(std::time::Duration::from_millis(12));
            }
        }
    }

    /// Write the `w`x`h` rectangle of B,G,R,X at (`x`,`y`) into the rgb24
    /// mirror. The band's rows are packed to `w` pixels, not to the screen's
    /// width, so the source stride comes from the band and the destination
    /// stride from the screen; conflating the two silently reads a shifted,
    /// sheared picture.
    fn apply(&self, x: usize, w: usize, y: usize, h: usize, rows: &[u8]) {
        let stride_in = w * BAND_BPP;
        let stride_out = self.width * RGB_BPP;
        if x + w > self.width || y + h > self.height || rows.len() < h * stride_in {
            eprintln!(
                "[screen] band x={x} w={w} y={y} h={h} does not fit {}x{} ({} bytes); skipped",
                self.width,
                self.height,
                rows.len()
            );
            return;
        }
        let mut f = self.frame.lock().unwrap();
        for row in 0..h {
            let src = &rows[row * stride_in..row * stride_in + stride_in];
            let start = (y + row) * stride_out + x * RGB_BPP;
            let dst = &mut f[start..start + w * RGB_BPP];
            for (px_in, px_out) in src.chunks_exact(BAND_BPP).zip(dst.chunks_exact_mut(RGB_BPP)) {
                px_out[0] = px_in[2]; // R
                px_out[1] = px_in[1]; // G
                px_out[2] = px_in[0]; // B
            }
        }
        drop(f);
        // Release, so a reader that sees this generation with an acquire load
        // is guaranteed to see the rows written above.
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// Pull a string field out of a flat JSON object without a JSON parser.
fn json_str<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":\"");
    let start = obj.find(&pat)? + pat.len();
    let rest = &obj[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn json_num(obj: &str, key: &str) -> Option<usize> {
    let pat = format!("\"{key}\":");
    let start = obj.find(&pat)? + pat.len();
    let rest = &obj[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | if c == b'=' { 0 } else { val(c)? };
        }
        let quad = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&quad[..3 - pad.min(3)]);
    }
    Some(out)
}
