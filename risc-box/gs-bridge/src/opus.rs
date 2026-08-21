// Opus encoding for the GameStream audio channel, and the pump that feeds it.
//
// Moonlight's audio is Opus at 48 kHz, and the machine's sound card plays
// whatever the guest asked for (DOOM asks for 11025 Hz stereo). So this pulls
// PCM from the app's /audio, resamples it to 48 kHz, and hands audio.rs 5 ms
// frames ready to packetize. libopus is linked from the system rather than
// vendored: it is the reference encoder, and the alternative is inventing a
// codec Moonlight would not understand.
//
// The pump POLLS rather than streaming. /audio is a destructive take, so one
// consumer owns it, and a poll every 40 ms costs 25 requests a second against
// an endpoint that measured clean at 10/s and is a GET besides (it was POSTs
// to /hid that used to hurt, and that is fixed). Latency is the poll interval
// plus a frame, which is well under what a player notices on a gunshot.

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

/// One second of 48 kHz stereo. Past this the host is not keeping up and the
/// oldest audio is the least worth hearing.
const RING_CAP: usize = OUT_RATE * OUT_CHANNELS;

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
}

impl AudioSource {
    pub fn new() -> AudioSource {
        AudioSource { buf: Arc::new(Mutex::new(VecDeque::new())) }
    }

    /// Take one frame's worth, or None if the guest has not produced that much
    /// yet — silence is the right filler and audio.rs already has it.
    pub fn take_frame(&self) -> Option<Vec<i16>> {
        let mut b = self.buf.lock().unwrap();
        let want = FRAME_SAMPLES * OUT_CHANNELS;
        if b.len() < want {
            return None;
        }
        Some(b.drain(..want).collect())
    }

    /// Poll the app's /audio and keep the buffer fed. Runs until the session
    /// stops.
    pub fn pump(self, session: Arc<Session>, app: Arc<App>) {
        while !session.is_stopping() {
            match app.get("/audio?max=65536") {
                Ok(body) => {
                    if let Some((rate, channels, pcm)) = parse_audio(&body) {
                        if !pcm.is_empty() {
                            let out = resample(&pcm, rate, channels);
                            let mut b = self.buf.lock().unwrap();
                            b.extend(out.iter().copied());
                            while b.len() > RING_CAP {
                                let over = b.len() - RING_CAP;
                                b.drain(..over);
                            }
                        }
                    }
                }
                Err(_) => {
                    // The app restarting is not fatal to the stream: the
                    // client keeps hearing silence and picks the audio back up
                    // when /audio answers again.
                    if !session.wait(Duration::from_millis(500)) {
                        return;
                    }
                }
            }
            if !session.wait(Duration::from_millis(40)) {
                return;
            }
        }
    }
}

/// Pull {"rate":N,"channels":N,...,"pcm":"<base64>"} apart without a JSON
/// parser — the shape is ours and fixed.
fn parse_audio(body: &[u8]) -> Option<(usize, usize, Vec<i16>)> {
    let text = std::str::from_utf8(body).ok()?;
    let rate = number_after(text, "\"rate\":")?;
    let channels = number_after(text, "\"channels\":")?;
    let start = text.find("\"pcm\":\"")? + 7;
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
