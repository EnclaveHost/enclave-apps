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
//! costs the guest ~6% against AV1's 20%. So: hold one long-lived connection,
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

    /// Copy the current picture out, in rgb24. Returns the generation so the
    /// caller can skip re-encoding an unchanged screen if it wants to.
    pub fn snapshot_into(&self, out: &mut Vec<u8>) -> u64 {
        let f = self.frame.lock().unwrap();
        out.clear();
        out.extend_from_slice(&f);
        self.generation.load(Ordering::Relaxed)
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
            let Some(b64) = json_str(payload, "b") else { continue };
            let (Some(y), Some(h)) = (json_num(payload, "y"), json_num(payload, "h")) else {
                continue;
            };
            let Some(deflated) = b64_decode(b64) else {
                eprintln!("[screen] band with undecodable base64, skipped");
                continue;
            };
            let Ok(rows) = miniz_oxide::inflate::decompress_to_vec(&deflated) else {
                eprintln!("[screen] band failed to inflate, skipped");
                continue;
            };
            self.apply(y, h, &rows);
        }
    }

    /// Write `h` rows of B,G,R,X starting at row `y` into the rgb24 mirror.
    fn apply(&self, y: usize, h: usize, rows: &[u8]) {
        let stride_in = self.width * BAND_BPP;
        let stride_out = self.width * RGB_BPP;
        if y + h > self.height || rows.len() < h * stride_in {
            eprintln!(
                "[screen] band y={y} h={h} does not fit {}x{} ({} bytes); skipped",
                self.width,
                self.height,
                rows.len()
            );
            return;
        }
        let mut f = self.frame.lock().unwrap();
        for row in 0..h {
            let src = &rows[row * stride_in..row * stride_in + stride_in];
            let dst = &mut f[(y + row) * stride_out..(y + row) * stride_out + stride_out];
            for (px_in, px_out) in src.chunks_exact(BAND_BPP).zip(dst.chunks_exact_mut(RGB_BPP)) {
                px_out[0] = px_in[2]; // R
                px_out[1] = px_in[1]; // G
                px_out[2] = px_in[0]; // B
            }
        }
        drop(f);
        self.generation.fetch_add(1, Ordering::Relaxed);
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
