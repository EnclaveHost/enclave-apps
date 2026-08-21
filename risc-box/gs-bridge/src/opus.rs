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
                let out = resample(&pcm, rate, channels);
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

/// Linear resample to 48 kHz stereo. Mono is duplicated across both channels,
/// which is what the card's own mono mode means.
fn resample(pcm: &[i16], rate: usize, channels: usize) -> Vec<i16> {
    if rate == 0 || channels == 0 || pcm.is_empty() {
        return Vec::new();
    }
    let in_frames = pcm.len() / channels;
    if in_frames < 2 {
        return Vec::new();
    }
    let out_frames = in_frames * OUT_RATE / rate;
    let mut out = Vec::with_capacity(out_frames * OUT_CHANNELS);
    for i in 0..out_frames {
        // Position in the input, 16.16 fixed point.
        let pos = ((i as u64) * (rate as u64) << 16) / OUT_RATE as u64;
        let idx = (pos >> 16) as usize;
        let frac = (pos & 0xffff) as i32;
        if idx + 1 >= in_frames {
            break;
        }
        for ch in 0..OUT_CHANNELS {
            let c = ch.min(channels - 1);
            let a = pcm[idx * channels + c] as i32;
            let b = pcm[(idx + 1) * channels + c] as i32;
            out.push((a + (((b - a) * frac) >> 16)) as i16);
        }
    }
    out
}
