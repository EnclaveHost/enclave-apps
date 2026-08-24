// Opus encoding for the GameStream audio channel, and the pump that feeds it.
//
// Moonlight's audio is Opus at 48 kHz, and the machine's sound card plays
// whatever the guest asked for (DOOM asks for 11025 Hz stereo). So this pulls
// PCM from the app's /audio, resamples it to 48 kHz, and hands audio.rs 5 ms
// frames ready to packetize. libopus is linked from the system rather than
// vendored: it is the reference encoder, and the alternative is inventing a
// codec Moonlight would not understand.
//
// The pump STREAMS, over the same SSE mechanism the video uses. It polled
// once, and that was audibly wrong: a request round trip per chunk is 100-400
// ms of relay jitter imposed on a stream consumed in 5 ms frames, so the
// listener alternately starved (heard as chopping) and sat on a backlog (heard
// as delay). Pushing means audio arrives as the card plays it, and every
// buffer between here and the guest can then be shallow.

use std::collections::VecDeque;
use std::os::raw::{c_int, c_void};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::app::App;
use crate::session::Session;

#[link(name = "opus")]
extern "C" {
    fn opus_encoder_create(fs: i32, channels: c_int, application: c_int,
                           error: *mut c_int) -> *mut c_void;
    fn opus_encode(st: *mut c_void, pcm: *const i16, frame_size: c_int,
                   data: *mut u8, max_data_bytes: i32) -> i32;
    fn opus_encoder_destroy(st: *mut c_void);
}

/// Low delay matters more than the last decibel for a game.
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;

pub const OUT_RATE: usize = 48_000;
pub const OUT_CHANNELS: usize = 2;
/// 5 ms at 48 kHz, per channel — the packet duration audio.rs advertises.
pub const FRAME_SAMPLES: usize = OUT_RATE / 1000 * 5;

/// ~200 ms of 48 kHz stereo. This is a jitter buffer, not a store: anything
/// held here is delay the player hears, and a deep one was most of why the
/// first cut sounded late.
const RING_CAP: usize = OUT_RATE * OUT_CHANNELS * 200 / 1000;

/// Audio starts flowing once this much is banked. It MUST exceed the interval
/// the app delivers on, or the buffer drains dry between chunks and every gap
/// is heard as a chop: measured delivery is ~41 ms typical and 84 ms worst,
/// so priming at 30 ms (as the first cut did) guaranteed the very problem the
/// buffer exists to prevent. 100 ms clears the worst case with room over.
const PRIME_SAMPLES: usize = OUT_RATE * OUT_CHANNELS * 100 / 1000;

pub struct Encoder {
    st: *mut c_void,
}

// The encoder is owned by exactly one thread (audio::run); the raw pointer is
// only unsendable because C says so.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Option<Encoder> {
        let mut err: c_int = 0;
        let st = unsafe {
            opus_encoder_create(OUT_RATE as i32, OUT_CHANNELS as c_int,
                                OPUS_APPLICATION_RESTRICTED_LOWDELAY, &mut err)
        };
        if st.is_null() || err != 0 {
            eprintln!("[audio] opus_encoder_create failed ({err})");
            return None;
        }
        Some(Encoder { st })
    }

    /// Encode one 5 ms stereo frame (FRAME_SAMPLES per channel, interleaved).
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Option<usize> {
        if pcm.len() < FRAME_SAMPLES * OUT_CHANNELS {
            return None;
        }
        let n = unsafe {
            opus_encode(self.st, pcm.as_ptr(), FRAME_SAMPLES as c_int,
                        out.as_mut_ptr(), out.len() as i32)
        };
        match n > 0 {
            true => Some(n as usize),
            false => None,
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.st) };
    }
}

/// PCM waiting to be encoded, in 48 kHz stereo.
#[derive(Clone)]
pub struct AudioSource {
    buf: Arc<Mutex<VecDeque<i16>>>,
    /// Whether the buffer has primed since it last ran dry. Emitting the
    /// instant the first samples land just moves the starvation one frame
    /// later; waiting for PRIME_SAMPLES rides out the jitter instead.
    primed: Arc<Mutex<bool>>,
}

impl AudioSource {
    pub fn new() -> AudioSource {
        AudioSource {
            buf: Arc::new(Mutex::new(VecDeque::new())),
            primed: Arc::new(Mutex::new(false)),
        }
    }

    /// Take one frame's worth, or None if the guest has not produced that much
    /// yet — silence is the right filler and audio.rs already has it.
    pub fn take_frame(&self) -> Option<Vec<i16>> {
        let mut b = self.buf.lock().unwrap();
        let mut primed = self.primed.lock().unwrap();
        let want = FRAME_SAMPLES * OUT_CHANNELS;
        if !*primed {
            if b.len() < PRIME_SAMPLES {
                return None;
            }
            *primed = true;
        }
        if b.len() < want {
            // Ran dry: prime again before resuming, or every following frame
            // is a coin toss between audio and silence.
            *primed = false;
            return None;
        }
        Some(b.drain(..want).collect())
    }

    /// Read the app's audio stream and keep the buffer fed. Runs until the
    /// session stops.
    pub fn pump(self, session: Arc<Session>, app: Arc<App>) {
        use std::io::BufRead;

        while !session.is_stopping() {
            let mut r = match app.get_stream("/audio?stream=1") {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[audio] /audio connect failed: {e}; retrying");
                    if !session.wait(Duration::from_secs(1)) {
                        return;
                    }
                    continue;
                }
            };
            let mut line = String::new();
            // One resampler per connection: it carries the fractional read
            // position and the boundary sample across chunks, so consecutive
            // /audio events join seamlessly. A fresh connection starts clean —
            // the guest audio has a gap across a redial anyway.
            let mut rs = Resampler::new();
            loop {
                if session.is_stopping() {
                    return;
                }
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) => break, // app closed the stream; redial
                    Ok(_) => {}
                    Err(_) => break,
                }
                let Some(payload) = line.strip_prefix("data: ") else { continue };
                let Some((rate, channels, pcm)) = parse_event(payload.trim()) else {
                    continue;
                };
                if pcm.is_empty() {
                    continue;
                }
                let out = rs.feed(&pcm, rate, channels);
                let mut b = self.buf.lock().unwrap();
                b.extend(out.iter().copied());
                while b.len() > RING_CAP {
                    let over = b.len() - RING_CAP;
                    b.drain(..over);
                }
            }
            // The app restarting is not fatal to the stream: the client keeps
            // hearing silence and the audio comes back when /audio does.
            if !session.wait(Duration::from_millis(500)) {
                return;
            }
        }
    }
}

/// Pull {"r":N,"c":N,"d":"<base64>"} apart without a JSON parser — the shape
/// is ours and fixed.
fn parse_event(text: &str) -> Option<(usize, usize, Vec<i16>)> {
    let rate = number_after(text, "\"r\":")?;
    let channels = number_after(text, "\"c\":")?;
    let start = text.find("\"d\":\"")? + 5;
    let end = text[start..].find('"')? + start;
    let raw = b64_decode(&text[start..end]);
    let mut pcm = Vec::with_capacity(raw.len() / 2);
    for c in raw.chunks_exact(2) {
        pcm.push(i16::from_le_bytes([c[0], c[1]]));
    }
    Some((rate, channels, pcm))
}

fn number_after(text: &str, key: &str) -> Option<usize> {
    let at = text.find(key)? + key.len();
    let rest = &text[at..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue, // '=' and any whitespace
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Continuous linear resampler to 48 kHz stereo. Mono is duplicated across
/// both channels, which is what the card's own mono mode means.
///
/// It is STATEFUL on purpose. Resampling each /audio chunk on its own restarts
/// the fractional read position at zero and drops the chunk's final sample
/// (there is nothing after it to interpolate toward), so every chunk boundary
/// was a step discontinuity — a click. /audio delivers many chunks a second,
/// so those clicks fused into a steady crackle on music and effects alike,
/// since both ride this one PCM stream. Carrying the phase and the last input
/// frame across chunks makes the boundaries seamless and holds the output rate
/// exactly at rate/48000 with no per-chunk truncation drift.
struct Resampler {
    /// 16.16 fixed-point read position. Index 0 is `last` (the previous chunk's
    /// final frame); index 1.. is the current chunk. Between chunks it holds the
    /// sub-sample remainder so the next chunk resumes exactly where this stopped.
    pos: u64,
    last: [i16; 2],
    primed: bool,
}

impl Resampler {
    fn new() -> Resampler {
        Resampler { pos: 0, last: [0, 0], primed: false }
    }

    fn feed(&mut self, pcm: &[i16], rate: usize, channels: usize) -> Vec<i16> {
        if rate == 0 || channels == 0 || channels > 2 {
            return Vec::new();
        }
        let n = pcm.len() / channels;
        if n == 0 {
            return Vec::new();
        }
        let frame = |i: usize| -> [i32; 2] {
            let l = pcm[i * channels] as i32;
            let r = if channels == 2 { pcm[i * channels + 1] as i32 } else { l };
            [l, r]
        };
        // Working frames: `last` prepended to the chunk, so the first outputs
        // interpolate across the boundary. The very first chunk has no `last`.
        let base = if self.primed { 1usize } else { 0 };
        let total = base + n;
        let get = |e: usize| -> [i32; 2] {
            if e < base {
                [self.last[0] as i32, self.last[1] as i32]
            } else {
                frame(e - base)
            }
        };
        let step = ((rate as u64) << 16) / OUT_RATE as u64;
        let mut out = Vec::new();
        loop {
            let idx = (self.pos >> 16) as usize;
            if idx + 1 >= total {
                break;
            }
            let frac = (self.pos & 0xffff) as i32;
            let a = get(idx);
            let b = get(idx + 1);
            out.push((a[0] + (((b[0] - a[0]) * frac) >> 16)) as i16);
            out.push((a[1] + (((b[1] - a[1]) * frac) >> 16)) as i16);
            self.pos += step;
        }
        // Carry state: the chunk's final frame becomes `last`, and pos is
        // rebased so index 0 maps to it again, keeping the sub-sample remainder
        // and any whole-frame overshoot past it (only possible when downsampling).
        let consumed = (self.pos >> 16) as usize;
        let overshoot = consumed.saturating_sub(total - 1) as u64;
        self.last = [
            pcm[(n - 1) * channels],
            if channels == 2 { pcm[(n - 1) * channels + 1] } else { pcm[(n - 1) * channels] },
        ];
        self.primed = true;
        self.pos = (overshoot << 16) | (self.pos & 0xffff);
        out
    }
}

#[cfg(test)]
mod resample_tests {
    use super::*;

    // A 300 Hz sine at 11025 Hz stereo, `n` frames from sample offset `off`.
    fn sine(off: usize, n: usize) -> Vec<i16> {
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = (off + i) as f64 / 11025.0;
            let s = (2.0 * std::f64::consts::PI * 300.0 * t).sin() * 12000.0;
            v.push(s as i16);
            v.push(s as i16);
        }
        v
    }

    // Feeding the same signal in many small chunks must match feeding it whole,
    // sample-for-sample — that is exactly the boundary continuity the crackle
    // came from lacking.
    #[test]
    fn chunked_matches_whole() {
        let total = 11025; // 1 second
        let whole = {
            let mut r = Resampler::new();
            r.feed(&sine(0, total), 11025, 2)
        };
        let chunked = {
            let mut r = Resampler::new();
            let mut out = Vec::new();
            let mut off = 0;
            // irregular chunk sizes, like real /audio bursts
            for &c in [441usize, 100, 512, 64, 900, 220, 1000].iter().cycle() {
                if off >= total { break; }
                let take = c.min(total - off);
                out.extend(r.feed(&sine(off, take), 11025, 2));
                off += take;
            }
            out
        };
        let common = whole.len().min(chunked.len());
        assert!(common > 40000, "expected ~96k samples, got {common}");
        // The chunk-vs-whole `last`-frame carry can differ by at most a quantization
        // step at boundaries; require exact equality, which the design gives.
        let mut maxdiff = 0i32;
        for i in 0..common {
            maxdiff = maxdiff.max((whole[i] as i32 - chunked[i] as i32).abs());
        }
        assert_eq!(maxdiff, 0, "chunked and whole diverged by {maxdiff}");
    }

    // No output sample should jump more than the input's own max slope between
    // adjacent samples allows — a boundary click shows up as an outsized step.
    #[test]
    fn no_boundary_discontinuity() {
        let mut r = Resampler::new();
        let mut out = Vec::new();
        let mut off = 0;
        for _ in 0..50 {
            out.extend(r.feed(&sine(off, 137), 11025, 2)); // odd chunk, exercises fractions
            off += 137;
        }
        // 300 Hz at 48 kHz: max per-sample step ~ 12000*2*pi*300/48000 ~ 471.
        // Allow generous headroom; a real click would be thousands.
        let mut worst = 0i32;
        for w in out.chunks_exact(2).collect::<Vec<_>>().windows(2) {
            worst = worst.max((w[1][0] as i32 - w[0][0] as i32).abs());
        }
        assert!(worst < 800, "suspicious step {worst} (boundary click?)");
    }
}
