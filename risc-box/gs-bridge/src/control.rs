// The control stream: ENet on :47999, AES-128-GCM in both directions.
//
// Every message is wrapped in the encrypted envelope (Sunshine
// stream.cpp:275-292 / moonlight-common-c ControlStream.c:11-32):
//
//   [type=0x0001 u16 LE][length u16 LE][seq u32 LE][GCM tag 16][ciphertext]
//
// decrypting to [type u16 LE][payloadLength u16 LE][payload]. The client
// enables encryption unconditionally at appversion >= 7.1.431, so there is
// no plaintext path to support.
//
// Input events arrive as control type 0x0206 and are translated here into
// the RISC Box app's POST /hid calls, which drive the emulator's
// virtio-input device.

use std::os::raw::c_void;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::crypto;
use crate::enet;
use crate::session::{Session, State};

const CTRL_ENCRYPTED: u16 = 0x0001;
const CTRL_PERIODIC_PING: u16 = 0x0200;
const CTRL_LOSS_STATS: u16 = 0x0201;
const CTRL_INVALIDATE_REF_FRAMES: u16 = 0x0301;
const CTRL_REQUEST_IDR: u16 = 0x0302;
const CTRL_START_B: u16 = 0x0307;
const CTRL_LTR_ACK: u16 = 0x0350;
const CTRL_INPUT_DATA: u16 = 0x0206;
const CTRL_TERMINATION: u16 = 0x0109;
const CTRL_FRAME_FEC_STATUS: u16 = 0x5502;

/// NVST_DISCONN_SERVER_TERMINATED_CLOSED — the graceful-shutdown code the
/// client maps to "session ended normally".
const TERMINATION_GRACEFUL: u32 = 0x8003_0023;

/// Input event magics (moonlight-common-c Input.h). `magic` is little-endian,
/// while `size` and most coordinate fields are big-endian.
const KEY_DOWN_EVENT_MAGIC: u32 = 0x0000_0003;
const KEY_UP_EVENT_MAGIC: u32 = 0x0000_0004;
const MOUSE_MOVE_ABS_MAGIC: u32 = 0x0000_0005;
const MOUSE_MOVE_REL_MAGIC_GEN5: u32 = 0x0000_0007;
const MOUSE_BUTTON_DOWN_MAGIC_GEN5: u32 = 0x0000_0008;
const MOUSE_BUTTON_UP_MAGIC_GEN5: u32 = 0x0000_0009;
const SCROLL_MAGIC_GEN5: u32 = 0x0000_000A;
const SS_HSCROLL_MAGIC: u32 = 0x5500_0001;
const UTF8_TEXT_EVENT_MAGIC: u32 = 0x0000_0017;

/// Wrap a plaintext control message in the encrypted envelope.
fn seal(session: &Session, msg_type: u16, payload: &[u8]) -> Vec<u8> {
    let seq = session.control_seq.fetch_add(1, Ordering::AcqRel);
    let iv = crypto::control_iv(seq, true);

    let mut plaintext = Vec::with_capacity(4 + payload.len());
    plaintext.extend_from_slice(&msg_type.to_le_bytes());
    plaintext.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    plaintext.extend_from_slice(payload);

    let (tag, ciphertext) = crypto::gcm_encrypt(&session.key, &iv, &plaintext);

    // length covers seq + tag + ciphertext.
    let length = (4 + 16 + ciphertext.len()) as u16;
    let mut out = Vec::with_capacity(4 + length as usize);
    out.extend_from_slice(&CTRL_ENCRYPTED.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&ciphertext);
    out
}

/// Unwrap an encrypted envelope from the client. Returns (type, payload).
fn unseal(session: &Session, data: &[u8]) -> Option<(u16, Vec<u8>)> {
    if data.len() < 8 {
        return None;
    }
    let header_type = u16::from_le_bytes([data[0], data[1]]);
    if header_type != CTRL_ENCRYPTED {
        // The client never sends plaintext once encryption is on.
        return None;
    }
    let length = u16::from_le_bytes([data[2], data[3]]) as usize;
    // seq(4) + tag(16) + at least a 4-byte inner header.
    if length < 24 || data.len() < 4 + length {
        return None;
    }
    let seq = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let tag = &data[8..24];
    let ciphertext = &data[24..4 + length];

    let iv = crypto::control_iv(seq, false);
    let plaintext = crypto::gcm_decrypt(&session.key, &iv, tag, ciphertext)?;
    if plaintext.len() < 4 {
        return None;
    }
    let msg_type = u16::from_le_bytes([plaintext[0], plaintext[1]]);
    Some((msg_type, plaintext[4..].to_vec()))
}

fn be16(b: &[u8], off: usize) -> i16 {
    i16::from_be_bytes([b[off], b[off + 1]])
}

/// Map a Windows virtual-key code to a Linux input-event keycode.
///
/// Moonlight sends Win32 VK codes interpreted on a US layout; the emulator's
/// virtio-input device speaks Linux keycodes, so the translation has to
/// happen here. Returns 0 for keys we have no equivalent for, which the app
/// ignores.
fn vk_to_linux_keycode(vk: u16) -> u16 {
    match vk {
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0D => 28,  // Enter
        0x10 | 0xA0 => 42, // Shift / LShift
        0xA1 => 54,  // RShift
        0x11 | 0xA2 => 29, // Ctrl / LCtrl
        0xA3 => 97,  // RCtrl
        0x12 | 0xA4 => 56, // Alt / LAlt
        0xA5 => 100, // RAlt
        0x14 => 58,  // Caps Lock
        0x1B => 1,   // Escape
        0x20 => 57,  // Space
        0x21 => 104, // Page Up
        0x22 => 109, // Page Down
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2D => 110, // Insert
        0x2E => 111, // Delete
        // Digit row: VK '1'..'9' are contiguous, '0' sits after them.
        0x30 => 11,
        0x31..=0x39 => (vk - 0x31) + 2,
        // Letters, in the order the Linux keymap lays out the rows.
        0x41 => 30, 0x42 => 48, 0x43 => 46, 0x44 => 32, 0x45 => 18,
        0x46 => 33, 0x47 => 34, 0x48 => 35, 0x49 => 23, 0x4A => 36,
        0x4B => 37, 0x4C => 38, 0x4D => 50, 0x4E => 49, 0x4F => 24,
        0x50 => 25, 0x51 => 16, 0x52 => 19, 0x53 => 31, 0x54 => 20,
        0x55 => 22, 0x56 => 47, 0x57 => 17, 0x58 => 45, 0x59 => 21,
        0x5A => 44,
        0x5B => 125, // Left Meta
        0x5C => 126, // Right Meta
        // Numpad
        0x60 => 82, 0x61 => 79, 0x62 => 80, 0x63 => 81, 0x64 => 75,
        0x65 => 76, 0x66 => 77, 0x67 => 71, 0x68 => 72, 0x69 => 73,
        0x6A => 55, 0x6B => 78, 0x6D => 74, 0x6E => 83, 0x6F => 98,
        // Function keys
        0x70..=0x79 => (vk - 0x70) + 59, // F1..F10
        0x7A => 87,  // F11
        0x7B => 88,  // F12
        0x90 => 69,  // Num Lock
        0x91 => 70,  // Scroll Lock
        // OEM punctuation, US layout
        0xBA => 39,  // ;:
        0xBB => 13,  // =+
        0xBC => 51,  // ,<
        0xBD => 12,  // -_
        0xBE => 52,  // .>
        0xBF => 53,  // /?
        0xC0 => 41,  // `~
        0xDB => 26,  // [{
        0xDC => 43,  // \|
        0xDD => 27,  // ]}
        0xDE => 40,  // '"
        _ => 0,
    }
}

/// One line per distinct unhandled input magic, then silence for it.
///
/// Input is a per-frame firehose, so this must never log per event; but a magic
/// that is never handled is a permanent hole and deserves to be said once, with
/// the body length, because the usual cause is a length guard rather than a
/// genuinely unknown event.
fn report_unhandled(magic: u32, body_len: usize) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<u32>>> = Mutex::new(None);
    let mut g = SEEN.lock().unwrap();
    let seen = g.get_or_insert_with(HashSet::new);
    if seen.insert(magic) {
        eprintln!(
            "[control] UNHANDLED input magic 0x{magic:08x} (body {body_len} bytes) \
             - these events are being dropped; the machine never sees them"
        );
    }
}

/// Build the /hid event object for one GameStream input event, or None if
/// the emulated HID has no equivalent.
///
/// The payload starts at NV_INPUT_HEADER: [size u32 BE][magic u32 LE].
/// Note the mixed endianness: mouse coordinates and scroll are big-endian,
/// the key code is little-endian.
fn input_event_json(session: &Session, payload: &[u8]) -> Option<String> {
    if payload.len() < 8 {
        return None;
    }
    let magic = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let body = &payload[8..];

    // GSB_INPUT_TRACE=1 dumps every input event's magic and leading body bytes.
    // Relative (captured-mouse) motion is exercised only under a real capture,
    // so this is the one reliable way to see what magic the client actually
    // sends for it — a frozen pointer is otherwise invisible from both ends.
    if std::env::var_os("GSB_INPUT_TRACE").is_some() {
        let n = body.len().min(12);
        eprintln!(
            "[trace] input magic 0x{magic:08x} body {} bytes: {:02x?}",
            body.len(),
            &body[..n]
        );
    }

    match magic {
        MOUSE_MOVE_REL_MAGIC_GEN5 if body.len() >= 4 => {
            // The emulated pointer is absolute, so integrate the delta into
            // the cursor we track. No event is queued: position is state, and
            // the drainer ships the latest once per cycle — forwarding every
            // 125-1000 Hz mouse sample drowned the emulated CPU in input IRQs
            // and froze the game for exactly as long as the mouse moved.
            let (dx, dy) = (be16(body, 0) as f64, be16(body, 2) as f64);
            let cfg = session.config.lock().unwrap().clone();
            let (w, h) = (cfg.width.max(1) as f64, cfg.height.max(1) as f64);
            let mut cur = session.cursor.lock().unwrap();
            cur.0 = (cur.0 + dx / w).clamp(0.0, 1.0);
            cur.1 = (cur.1 + dy / h).clamp(0.0, 1.0);
            drop(cur);
            session.cursor_dirty.store(true, Ordering::Release);
            None
        }
        MOUSE_MOVE_ABS_MAGIC if body.len() >= 10 => {
            // x/y are in the client's reference space, whose inclusive bounds
            // are the width/height fields (the client already decremented them).
            let (x, y) = (be16(body, 0) as f64, be16(body, 2) as f64);
            let (ref_w, ref_h) = (be16(body, 6) as f64, be16(body, 8) as f64);
            if ref_w <= 0.0 || ref_h <= 0.0 {
                return None;
            }
            let nx = (x / ref_w).clamp(0.0, 1.0);
            let ny = (y / ref_h).clamp(0.0, 1.0);
            *session.cursor.lock().unwrap() = (nx, ny);
            session.cursor_dirty.store(true, Ordering::Release);
            None
        }
        MOUSE_BUTTON_DOWN_MAGIC_GEN5 | MOUSE_BUTTON_UP_MAGIC_GEN5 if !body.is_empty() => {
            let down = magic == MOUSE_BUTTON_DOWN_MAGIC_GEN5;
            // BUTTON_LEFT 1, MIDDLE 2, RIGHT 3; X1/X2 have no equivalent.
            let name = match body[0] {
                1 => "left",
                2 => "middle",
                3 => "right",
                _ => return None,
            };
            Some(format!(r#"{{"t":"button","b":"{name}","down":{down}}}"#))
        }
        SCROLL_MAGIC_GEN5 if body.len() >= 2 => {
            // High-resolution units, 120 per notch; the guest wants notches.
            let amt = be16(body, 0) as i32;
            let notches = notches_from_high_res(amt);
            if notches == 0 {
                return None;
            }
            Some(format!(r#"{{"t":"scroll","dy":{notches}}}"#))
        }
        SS_HSCROLL_MAGIC if body.len() >= 2 => {
            let amt = be16(body, 0) as i32;
            let notches = notches_from_high_res(amt);
            if notches == 0 {
                return None;
            }
            Some(format!(r#"{{"t":"scroll","dx":{notches}}}"#))
        }
        KEY_DOWN_EVENT_MAGIC | KEY_UP_EVENT_MAGIC if body.len() >= 4 => {
            // [flags u8][keyCode u16 LE][modifiers u8]
            //
            // moonlight-qt sets bit 0x8000 on the key code of every keypress
            // it sends (keyboard.cpp: `LiSendKeyboardEvent(0x8000 | keyCode…)`);
            // Sunshine masks it off and so must we, or every real keystroke
            // looks up as 0x80xx, misses the table, and is silently dropped —
            // which is exactly the "keyboard does nothing" symptom, while a
            // test client sending the bare VK code works. Strip the flag before
            // the lookup. (Some paths, e.g. release-on-focus-loss, send the
            // bare code; masking leaves those unchanged.)
            let vk = u16::from_le_bytes([body[1], body[2]]) & 0x7FFF;
            let code = vk_to_linux_keycode(vk);
            if code == 0 {
                return None;
            }
            let down = magic == KEY_DOWN_EVENT_MAGIC;
            Some(format!(r#"{{"t":"key","code":{code},"down":{down}}}"#))
        }
        // Gamepad, touch, pen, UTF-8 text and haptics: the emulated HID has
        // no equivalent device, so these are accepted and dropped rather
        // than erroring.
        // NOT silently dropped. A magic we do not handle -- or one we do whose
        // body failed its length guard above and fell through to here -- means
        // input the client sent and the machine never saw. That is invisible
        // from both ends: the client believes it sent it, the app never hears
        // it, and the user reports a frozen pointer with nothing in any log.
        // Relative motion in particular is only exercised when the client
        // CAPTURES the mouse, which windowed testing never does.
        other => {
            report_unhandled(other, body.len());
            None
        }
    }
}

/// Convert Moonlight's high-resolution scroll units (120 per notch) into
/// whole notches, keeping at least one notch for any nonzero motion so small
/// trackpad scrolls are not swallowed.
fn notches_from_high_res(amount: i32) -> i32 {
    if amount == 0 {
        return 0;
    }
    let n = amount / 120;
    if n != 0 {
        n
    } else if amount > 0 {
        1
    } else {
        -1
    }
}

/// Queue of input events waiting to be handed to the machine.
///
/// Driving a desktop produces a flood of pointer motion, and posting each
/// event on its own connection would spend more time in TCP setup than in the
/// emulator. The app's /hid takes a batch, so events accumulate here and a
/// drainer ships them together.
#[derive(Default)]
pub struct InputQueue {
    events: Mutex<Vec<String>>,
    cv: Condvar,
}

impl InputQueue {
    fn push(&self, event: String) {
        let mut q = self.events.lock().unwrap();
        // Pointer motion is absolute and idempotent: if a move is already
        // pending, the newer position supersedes it rather than replaying a
        // path the guest would only have to redraw twice.
        if event.contains(r#""t":"move""#) {
            if let Some(last) = q.last_mut() {
                if last.contains(r#""t":"move""#) {
                    *last = event;
                    self.cv.notify_one();
                    return;
                }
            }
        }
        q.push(event);
        self.cv.notify_one();
    }

    /// Wait briefly for events and return everything queued.
    fn drain(&self, wait: Duration) -> Vec<String> {
        let q = self.events.lock().unwrap();
        let (mut q, _) = self.cv.wait_timeout_while(q, wait, |q| q.is_empty()).unwrap();
        std::mem::take(&mut *q)
    }
}

/// Ship queued input into the machine until the session ends.
///
/// Between bursts it keeps one pooled connection warm with a cheap `/ping`:
/// the relay cuts idle kept-alive connections, and without the ping the first
/// event of every gesture pays a discovered-dead-socket plus a fresh TCP+TLS
/// dial before it moves. Input latency is judged by exactly that first event.
/// How often a pure pointer move is shipped to the app.
///
/// Every accepted POST /hid boosts the emulator loop, and the boost is what
/// starves the display worker: the app's own measurement is that 10 accepted
/// /hid a second took videoFps from 40 to 0 with turnMax still healthy. So this
/// is not a smoothness knob, it is the throttle that decides whether the
/// picture keeps moving while the pointer does. GSB_CURSOR_MS overrides it.
fn cursor_pace() -> Duration {
    std::env::var("GSB_CURSOR_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(100))
}

fn input_drainer(session: Arc<Session>, app: Arc<App>, queue: Arc<InputQueue>) {
    const KEEPALIVE: Duration = Duration::from_secs(15);
    // The streamed channel first: one POST /hid-stream whose chunked body
    // carries every batch as a line — no per-batch request framing and no
    // responses at all. A modern app never answers it, so the moment the
    // peer says ANYTHING (an old app 404ing, a proxy erroring) this drops to
    // the pipelined per-request channel, which every app speaks.
    let viable = app.input_stream_viable();
    let mut stream = if viable { Some(app.input_stream()) } else { None };
    let mut pipe: Option<crate::app::InputPipe> =
        if viable { None } else { Some(app.input_pipe()) };
    if !viable {
        eprintln!("[control] input: pipelined /hid (streamed channel is loopback-only)");
    }
    // A lost stream is a fallback, not a verdict: the platform kills long-held
    // request streams periodically (lease-slice churn), and against an OLD app
    // the answer arrives on every dial. So the pipe carries input while a
    // fresh dial sits a 1s probation holding only a benign empty line; silence
    // promotes it back, an answer reschedules with backoff (5s..60s), which
    // against a genuinely old app costs one idle connection a minute.
    let mut candidate: Option<(crate::app::InputStream, std::time::Instant)> = None;
    let mut retry_at: Option<std::time::Instant> = None;
    let mut retry_backoff = Duration::from_secs(5);
    let mut last_used = std::time::Instant::now();
    let mut last_cursor: Option<std::time::Instant> = None;
    // Minimum key hold. A game reads HELD key state once per game tic; on a
    // slow guest (this emulated desktop runs DOOM at ~8 fps, ~125 ms/tic) a
    // quick tap's key-down and key-up can both land between two tics, so
    // per-tic-sampled actions (fire on Ctrl, use on Space) never register even
    // though every edge-triggered key — menu, automap, weapon numbers — does.
    // Hold each key down for at least one tic by deferring its key-up until
    // KEY_MIN_HOLD after the key-down. A real hold releases long after this, so
    // it is untouched; only sub-tic taps are stretched. 0 disables it.
    let key_min_hold = std::env::var("GSB_KEY_MIN_HOLD_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(160));
    let mut key_down_at: std::collections::HashMap<u16, std::time::Instant> =
        std::collections::HashMap::new();
    let mut deferred_ups: Vec<(u16, std::time::Instant)> = Vec::new();
    while !session.is_stopping() {
        if stream.is_none() {
            if let Some((mut cand, since)) = candidate.take() {
                if cand.answered() {
                    retry_at = Some(std::time::Instant::now() + retry_backoff);
                    retry_backoff = (retry_backoff * 2).min(Duration::from_secs(60));
                } else if since.elapsed() >= Duration::from_secs(1) {
                    eprintln!("[control] /hid-stream restored");
                    stream = Some(cand);
                    pipe = None;
                    retry_backoff = Duration::from_secs(5);
                } else {
                    candidate = Some((cand, since));
                }
            } else if viable && retry_at.is_some_and(|t| std::time::Instant::now() >= t) {
                retry_at = None;
                let mut cand = app.input_stream();
                match cand.send(r#"{"events":[]}"#) {
                    Ok(()) => candidate = Some((cand, std::time::Instant::now())),
                    Err(_) => {
                        retry_at = Some(std::time::Instant::now() + retry_backoff);
                        retry_backoff = (retry_backoff * 2).min(Duration::from_secs(60));
                    }
                }
            }
        }
        let mut batch = queue.drain(Duration::from_millis(50));
        // The coalesced pointer: at most one position update per cycle, the
        // latest, ahead of any queued clicks so they land where the cursor
        // is. Pure motion (nothing else queued) ships at half cadence — the
        // cursor serves desktop clicks, not the game, and every request is
        // load on the box the video stream shares.
        if session.cursor_dirty.load(Ordering::Acquire) {
            let due = !batch.is_empty()
                || last_cursor.map_or(true, |t: std::time::Instant| {
                    t.elapsed() >= cursor_pace()
                });
            if due {
                session.cursor_dirty.store(false, Ordering::Release);
                let cur = *session.cursor.lock().unwrap();
                batch.insert(0, format!(r#"{{"t":"move","x":{:.6},"y":{:.6}}}"#, cur.0, cur.1));
                last_cursor = Some(std::time::Instant::now());
            }
        }
        // Enforce the minimum key hold: record key-downs, hold back a key-up
        // that arrives too soon after its down, and release any deferred ups
        // whose hold has elapsed. The 50 ms drain keeps this loop turning even
        // with no new input, so a deferred up flushes within ~50 ms of due.
        if key_min_hold > Duration::ZERO {
            let parse_key = |ev: &str| -> Option<(u16, bool)> {
                if !ev.contains(r#""t":"key""#) {
                    return None;
                }
                let code = ev
                    .split(r#""code":"#)
                    .nth(1)?
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse()
                    .ok()?;
                Some((code, ev.contains(r#""down":true"#)))
            };
            let now = std::time::Instant::now();
            let mut kept: Vec<String> = Vec::with_capacity(batch.len());
            for ev in batch.drain(..) {
                match parse_key(&ev) {
                    Some((code, true)) => {
                        key_down_at.insert(code, now);
                        kept.push(ev);
                    }
                    Some((code, false)) => match key_down_at.get(&code) {
                        Some(&down) if now.duration_since(down) < key_min_hold => {
                            deferred_ups.push((code, down + key_min_hold));
                        }
                        _ => kept.push(ev),
                    },
                    None => kept.push(ev),
                }
            }
            batch = kept;
            let now = std::time::Instant::now();
            deferred_ups.retain(|&(code, release_at)| {
                if now >= release_at {
                    batch.push(format!(r#"{{"t":"key","code":{code},"down":false}}"#));
                    false
                } else {
                    true
                }
            });
        }
        if batch.is_empty() {
            if let Some(p) = pipe.as_mut() {
                p.poll();
                if last_used.elapsed() >= KEEPALIVE {
                    p.send("GET", "/ping", &[]);
                    last_used = std::time::Instant::now();
                }
            }
            if let Some(st) = stream.as_mut() {
                if st.answered() {
                    eprintln!("[control] /hid-stream answered by the app — falling back to pipelined /hid");
                    stream = None;
                    pipe = Some(app.input_pipe());
                    retry_at = Some(std::time::Instant::now() + retry_backoff);
                } else if last_used.elapsed() >= KEEPALIVE {
                    let _ = st.send(r#"{"events":[]}"#);
                    last_used = std::time::Instant::now();
                }
            }
            continue;
        }
        let body = format!(r#"{{"events":[{}]}}"#, batch.join(","));
        if std::env::var_os("GSB_DEBUG_INPUT").is_some() {
            eprintln!("[control] input {} events: {body}", batch.len());
        }
        match (stream.as_mut(), pipe.as_mut()) {
            (Some(st), _) => {
                if st.send(&body).is_err() || st.answered() {
                    eprintln!("[control] /hid-stream unavailable — falling back to pipelined /hid");
                    stream = None;
                    let mut p = app.input_pipe();
                    p.send("POST", "/hid", body.as_bytes());
                    pipe = Some(p);
                    retry_at = Some(std::time::Instant::now() + retry_backoff);
                }
            }
            (None, Some(p)) => p.send("POST", "/hid", body.as_bytes()),
            (None, None) => unreachable!(),
        }
        last_used = std::time::Instant::now();
    }
}

/// Translate a GameStream input event and queue it for the machine.
fn handle_input(queue: &InputQueue, session: &Session, payload: &[u8]) {
    if let Some(event) = input_event_json(session, payload) {
        queue.push(event);
    }
}

/// Send the graceful-termination message and disconnect the peer.
pub fn terminate(session: &Session, peer: *mut c_void) {
    let payload = TERMINATION_GRACEFUL.to_be_bytes();
    let msg = seal(session, CTRL_TERMINATION, &payload);
    enet::send_reliable(peer, 0, &msg);
}

/// Run the ENet control server until the session ends.
///
/// This owns the ENet host, so all peer operations happen on this thread —
/// ENet hosts are not thread-safe.
pub fn run(session: Arc<Session>, app: Arc<App>, on_running: impl Fn()) {
    enet::init();

    let addr = enet::ENetAddress::any_v4(crate::session::PORT_CONTROL);
    let host = unsafe { enet::enet_host_create(enet::AF_INET, &addr, 128, 0, 0, 0) };
    if host.is_null() {
        eprintln!("[control] failed to bind ENet on :{}", crate::session::PORT_CONTROL);
        session.stop();
        return;
    }
    eprintln!("[control] ENet listening on :{}", crate::session::PORT_CONTROL);

    // Input is queued here and shipped in batches; see InputQueue.
    let queue = Arc::new(InputQueue::default());
    {
        let (s, a, q) = (session.clone(), app.clone(), queue.clone());
        std::thread::spawn(move || input_drainer(s, a, q));
    }

    let mut peer: *mut c_void = std::ptr::null_mut();
    let mut ping_deadline = Instant::now() + Duration::from_secs(30);
    let mut notified_running = false;
    let mut fec_reports = FecReports::default();

    loop {
        if session.is_stopping() {
            break;
        }

        let mut event = enet::ENetEvent::default();
        let rc = unsafe { enet::enet_host_service(host, &mut event, 150) };
        if rc < 0 {
            eprintln!("[control] enet_host_service failed");
            break;
        }

        if rc > 0 {
            // Any traffic from the peer counts as liveness.
            ping_deadline = Instant::now() + Duration::from_secs(10);

            match event.kind {
                enet::ENET_EVENT_TYPE_CONNECT => {
                    // The connect data must match the token we handed out in
                    // RTSP SETUP, otherwise this is not our client.
                    if event.data != session.connect_data {
                        eprintln!(
                            "[control] rejecting connect: data {:#x} != expected {:#x}",
                            event.data, session.connect_data
                        );
                        unsafe { enet::enet_peer_disconnect_now(event.peer, 0) };
                        continue;
                    }
                    eprintln!("[control] *** CLIENT CONNECTED ***");
                    peer = event.peer;
                    unsafe { enet::enet_peer_timeout(peer, 2, 10_000, 10_000) };
                    *session.state.lock().unwrap() = State::Running;
                    // Video can only start once we have a control peer.
                    if !notified_running {
                        notified_running = true;
                        on_running();
                    }
                }
                enet::ENET_EVENT_TYPE_DISCONNECT => {
                    eprintln!("[control] client disconnected");
                    peer = std::ptr::null_mut();
                    session.stop();
                    break;
                }
                enet::ENET_EVENT_TYPE_RECEIVE => {
                    let data = unsafe {
                        std::slice::from_raw_parts(
                            (*event.packet).data,
                            (*event.packet).data_length,
                        )
                        .to_vec()
                    };
                    unsafe { enet::enet_packet_destroy(event.packet) };

                    match unseal(&session, &data) {
                        Some((msg_type, payload)) => match msg_type {
                            CTRL_INPUT_DATA => handle_input(&queue, &session, &payload),
                            CTRL_REQUEST_IDR => {
                                eprintln!("[control] IDR requested");
                                session.request_idr();
                            }
                            CTRL_INVALIDATE_REF_FRAMES => session.request_idr(),
                            // The client's own account of a frame it could not
                            // assemble. This is the only direct evidence of
                            // WHY a stream is dropping frames, so it is worth
                            // decoding rather than discarding.
                            CTRL_FRAME_FEC_STATUS => fec_reports.record(&payload),
                            // Keepalives and telemetry: nothing to do, but
                            // receiving them is what keeps the peer alive.
                            // (CTRL_LOSS_STATS carries a hardcoded zero loss
                            // count on current clients; the FEC status above
                            // is the one with real numbers in it.)
                            CTRL_PERIODIC_PING
                            | CTRL_LOSS_STATS
                            | CTRL_START_B
                            | CTRL_LTR_ACK => {}
                            other => {
                                eprintln!("[control] unhandled message type {other:#06x}");
                            }
                        },
                        None => {
                            eprintln!("[control] failed to decrypt a control message");
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        // Service returned idle; enforce the ping timeout.
        if !peer.is_null() && Instant::now() > ping_deadline {
            eprintln!("[control] ping timeout, ending session");
            session.stop();
            break;
        }
    }

    if !peer.is_null() {
        terminate(&session, peer);
        unsafe {
            enet::enet_host_flush(host);
            enet::enet_peer_disconnect_now(peer, 0);
        }
    }
    unsafe { enet::enet_host_destroy(host) };
    fec_reports.summarize();
    eprintln!("[control] stopped");
}

/// One SS_FRAME_FEC_STATUS, as the client packs it (moonlight-common-c
/// Video.h — packed, big-endian throughout).
///
/// The client only sends this when a frame needed FEC recovery or had to be
/// abandoned, so its mere arrival means that frame was damaged. What it tells
/// us that nothing else does is WHERE the damage was: `received_data` short of
/// `total_data` with the parity also short means packets genuinely went
/// missing on the wire, while full counts with a bad sequence number means the
/// host built the frame wrong.
#[derive(Debug, Clone, Copy)]
struct FecStatus {
    frame_index: u32,
    highest_seq: u16,
    next_contiguous_seq: u16,
    missing_before_highest: u16,
    total_data: u16,
    total_parity: u16,
    received_data: u16,
    received_parity: u16,
    fec_percentage: u8,
    block_index: u8,
    block_count: u8,
}

impl FecStatus {
    /// 4 + 2*7 + 3 = 21 bytes.
    const WIRE_LEN: usize = 21;

    fn parse(p: &[u8]) -> Option<FecStatus> {
        if p.len() < Self::WIRE_LEN {
            return None;
        }
        let be16 = |i: usize| u16::from_be_bytes([p[i], p[i + 1]]);
        Some(FecStatus {
            frame_index: u32::from_be_bytes([p[0], p[1], p[2], p[3]]),
            highest_seq: be16(4),
            next_contiguous_seq: be16(6),
            missing_before_highest: be16(8),
            total_data: be16(10),
            total_parity: be16(12),
            received_data: be16(14),
            received_parity: be16(16),
            fec_percentage: p[18],
            block_index: p[19],
            block_count: p[20],
        })
    }

    /// True when every shard we sent for this block did arrive. A damaged
    /// frame with nothing missing is the host's fault, not the network's.
    fn nothing_missing(&self) -> bool {
        self.received_data >= self.total_data && self.received_parity >= self.total_parity
    }
}

/// Running tally of the client's damage reports, so a session ends with a
/// verdict instead of a wall of per-frame lines.
#[derive(Default)]
struct FecReports {
    count: u64,
    lost_shards: u64,
    complete_but_damaged: u64,
    logged: u64,
}

impl FecReports {
    fn record(&mut self, payload: &[u8]) {
        let Some(s) = FecStatus::parse(payload) else {
            eprintln!("[control] FEC status too short ({} bytes)", payload.len());
            return;
        };
        self.count += 1;
        let missing_data = s.total_data.saturating_sub(s.received_data) as u64;
        let missing_parity = s.total_parity.saturating_sub(s.received_parity) as u64;
        self.lost_shards += missing_data + missing_parity;
        if s.nothing_missing() {
            self.complete_but_damaged += 1;
        }
        // The first few in full, then only a summary: a stream that is losing
        // every frame would otherwise bury everything else in the log.
        if self.logged < 10 {
            self.logged += 1;
            eprintln!(
                "[control] frame {} damaged: data {}/{}, parity {}/{}, missing-before-highest {}, \
                 seq next={} highest={}, fec {}%, block {}/{}",
                s.frame_index,
                s.received_data,
                s.total_data,
                s.received_parity,
                s.total_parity,
                s.missing_before_highest,
                s.next_contiguous_seq,
                s.highest_seq,
                s.fec_percentage,
                s.block_index,
                s.block_count,
            );
        }
    }

    fn summarize(&self) {
        if self.count == 0 {
            eprintln!("[control] client reported no damaged frames");
            return;
        }
        eprintln!(
            "[control] client reported {} damaged frame(s), {} shard(s) never arrived, \
             {} damaged with every shard present",
            self.count, self.lost_shards, self.complete_but_damaged
        );
        if self.complete_but_damaged > 0 {
            eprintln!(
                "[control] frames damaged with nothing missing point at how this host \
                 packetizes, not at the network"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        Session::new(1, vec![0x11u8; 16], 0x2233_4455, "0123456789abcdef".into(), 0xDEAD_BEEF, 0)
    }

    #[test]
    fn envelope_round_trips_through_the_client_iv_scheme() {
        let s = test_session();
        // Seal as the host, then unseal using the host IV to confirm the
        // framing; direction separation is checked below.
        let msg = seal(&s, CTRL_TERMINATION, &TERMINATION_GRACEFUL.to_be_bytes());

        assert_eq!(u16::from_le_bytes([msg[0], msg[1]]), CTRL_ENCRYPTED);
        let length = u16::from_le_bytes([msg[2], msg[3]]) as usize;
        assert_eq!(msg.len(), 4 + length, "wire length must be 4 + length field");
        // seq(4) + tag(16) + inner header(4) + payload(4)
        assert_eq!(length, 4 + 16 + 4 + 4);

        let seq = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]);
        let iv = crypto::control_iv(seq, true);
        let plain = crypto::gcm_decrypt(&s.key, &iv, &msg[8..24], &msg[24..]).expect("decrypts");
        assert_eq!(u16::from_le_bytes([plain[0], plain[1]]), CTRL_TERMINATION);
        assert_eq!(u16::from_le_bytes([plain[2], plain[3]]), 4);
        assert_eq!(&plain[4..], &TERMINATION_GRACEFUL.to_be_bytes());
    }

    #[test]
    fn host_and_client_ivs_differ_only_in_the_origin_byte() {
        let h = crypto::control_iv(7, true);
        let c = crypto::control_iv(7, false);
        assert_eq!(h[0..4], 7u32.to_le_bytes());
        assert_eq!(h[4..10], [0u8; 6]);
        assert_eq!(h[10], b'H');
        assert_eq!(c[10], b'C');
        assert_eq!(h[11], b'C');
        assert_eq!(c[11], b'C');
    }

    #[test]
    fn unseal_accepts_a_client_sealed_message() {
        let s = test_session();
        // Build what the client would send: seq counter of its own, 'C' origin.
        let seq = 3u32;
        let iv = crypto::control_iv(seq, false);
        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(&CTRL_REQUEST_IDR.to_le_bytes());
        plaintext.extend_from_slice(&2u16.to_le_bytes());
        plaintext.extend_from_slice(&[0, 0]);
        let (tag, ct) = crypto::gcm_encrypt(&s.key, &iv, &plaintext);

        let mut wire = Vec::new();
        wire.extend_from_slice(&CTRL_ENCRYPTED.to_le_bytes());
        wire.extend_from_slice(&((4 + 16 + ct.len()) as u16).to_le_bytes());
        wire.extend_from_slice(&seq.to_le_bytes());
        wire.extend_from_slice(&tag);
        wire.extend_from_slice(&ct);

        let (t, payload) = unseal(&s, &wire).expect("unseals");
        assert_eq!(t, CTRL_REQUEST_IDR);
        assert_eq!(payload, vec![0, 0]);
    }

    /// Build an input payload: [size u32 BE][magic u32 LE][body].
    fn input_payload(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        p.extend_from_slice(&magic.to_le_bytes());
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn absolute_mouse_maps_into_normalized_coordinates() {
        // Moves are coalesced: the event updates cursor state and queues
        // nothing; the drainer ships the latest position once per cycle.
        let s = test_session();
        s.config.lock().unwrap().width = 1280;
        s.config.lock().unwrap().height = 720;

        // Centre of a 1279x719 reference space (the client sends dims - 1).
        let mut body = Vec::new();
        body.extend_from_slice(&639i16.to_be_bytes()); // x
        body.extend_from_slice(&359i16.to_be_bytes()); // y
        body.extend_from_slice(&0i16.to_be_bytes()); // unused
        body.extend_from_slice(&1279i16.to_be_bytes()); // width
        body.extend_from_slice(&719i16.to_be_bytes()); // height

        let queued = input_event_json(&s, &input_payload(MOUSE_MOVE_ABS_MAGIC, &body));
        assert!(queued.is_none(), "moves are state, not queue entries: {queued:?}");
        assert!(s.cursor_dirty.load(std::sync::atomic::Ordering::Acquire));
        let cur = *s.cursor.lock().unwrap();
        assert!((cur.0 - 0.4996).abs() < 1e-3, "x should be ~0.5: {cur:?}");
    }

    #[test]
    fn relative_mouse_is_integrated_into_an_absolute_position() {
        let s = test_session();
        s.config.lock().unwrap().width = 1000;
        s.config.lock().unwrap().height = 1000;
        *s.cursor.lock().unwrap() = (0.5, 0.5);

        let mut body = Vec::new();
        body.extend_from_slice(&100i16.to_be_bytes()); // dx
        body.extend_from_slice(&(-250i16).to_be_bytes()); // dy
        let queued = input_event_json(&s, &input_payload(MOUSE_MOVE_REL_MAGIC_GEN5, &body));
        assert!(queued.is_none(), "moves are state, not queue entries: {queued:?}");
        assert!(s.cursor_dirty.load(std::sync::atomic::Ordering::Acquire));

        let cur = *s.cursor.lock().unwrap();
        assert!((cur.0 - 0.6).abs() < 1e-6, "x should advance by dx/width: {cur:?}");
        assert!((cur.1 - 0.25).abs() < 1e-6, "y should advance by dy/height: {cur:?}");
    }

    #[test]
    fn buttons_use_the_names_the_app_expects() {
        let s = test_session();
        for (id, name) in [(1u8, "left"), (2, "middle"), (3, "right")] {
            let json = input_event_json(&s, &input_payload(MOUSE_BUTTON_DOWN_MAGIC_GEN5, &[id]))
                .expect("event");
            assert!(json.contains(&format!(r#""b":"{name}""#)), "{json}");
            assert!(json.contains(r#""down":true"#));
        }
        // X1/X2 have no emulated equivalent and must not be invented.
        assert!(input_event_json(&s, &input_payload(MOUSE_BUTTON_DOWN_MAGIC_GEN5, &[4])).is_none());
    }

    #[test]
    fn keyboard_translates_windows_vk_to_linux_keycodes() {
        let s = test_session();
        // 'A' (VK 0x41) is Linux KEY_A == 30, not 65.
        let body = [0u8, 0x41, 0x00, 0x00];
        let json = input_event_json(&s, &input_payload(KEY_DOWN_EVENT_MAGIC, &body)).expect("event");
        assert!(json.contains(r#""code":30"#), "expected KEY_A=30: {json}");

        // A few more anchors across the table.
        assert_eq!(vk_to_linux_keycode(0x1B), 1, "Escape");
        assert_eq!(vk_to_linux_keycode(0x0D), 28, "Enter");
        assert_eq!(vk_to_linux_keycode(0x20), 57, "Space");
        assert_eq!(vk_to_linux_keycode(0x31), 2, "digit 1");
        assert_eq!(vk_to_linux_keycode(0x30), 11, "digit 0");
        assert_eq!(vk_to_linux_keycode(0x70), 59, "F1");
        assert_eq!(vk_to_linux_keycode(0x7B), 88, "F12");
        assert_eq!(vk_to_linux_keycode(0x26), 103, "Up arrow");
        assert_eq!(vk_to_linux_keycode(0x5A), 44, "Z");
        assert_eq!(vk_to_linux_keycode(0x00), 0, "unmapped keys are dropped");
    }

    #[test]
    fn scroll_converts_high_resolution_units_to_notches() {
        assert_eq!(notches_from_high_res(120), 1);
        assert_eq!(notches_from_high_res(-120), -1);
        assert_eq!(notches_from_high_res(360), 3);
        // Sub-notch motion still moves, rather than being swallowed.
        assert_eq!(notches_from_high_res(30), 1);
        assert_eq!(notches_from_high_res(-30), -1);
        assert_eq!(notches_from_high_res(0), 0);

        let s = test_session();
        let json = input_event_json(&s, &input_payload(SCROLL_MAGIC_GEN5, &120i16.to_be_bytes()))
            .expect("event");
        assert!(json.contains(r#""t":"scroll""#) && json.contains(r#""dy":1"#), "{json}");
    }

    #[test]
    fn unseal_rejects_a_tampered_tag() {
        let s = test_session();
        let seq = 1u32;
        let iv = crypto::control_iv(seq, false);
        let plaintext = [0x06, 0x02, 0x00, 0x00];
        let (mut tag, ct) = crypto::gcm_encrypt(&s.key, &iv, &plaintext);
        tag[0] ^= 0xFF;

        let mut wire = Vec::new();
        wire.extend_from_slice(&CTRL_ENCRYPTED.to_le_bytes());
        wire.extend_from_slice(&((4 + 16 + ct.len()) as u16).to_le_bytes());
        wire.extend_from_slice(&seq.to_le_bytes());
        wire.extend_from_slice(&tag);
        wire.extend_from_slice(&ct);

        assert!(unseal(&s, &wire).is_none(), "a forged tag must not authenticate");
    }

    /// The FEC status is packed big-endian in field order. Reading it with the
    /// wrong endianness still "parses" — it just reports nonsense — so pin the
    /// layout against a hand-built buffer.
    #[test]
    fn fec_status_parses_the_clients_big_endian_layout() {
        let mut w = Vec::new();
        w.extend_from_slice(&1234u32.to_be_bytes()); // frameIndex
        w.extend_from_slice(&600u16.to_be_bytes()); // highestReceivedSequenceNumber
        w.extend_from_slice(&598u16.to_be_bytes()); // nextContiguousSequenceNumber
        w.extend_from_slice(&2u16.to_be_bytes()); // missingPacketsBeforeHighestReceived
        w.extend_from_slice(&3u16.to_be_bytes()); // totalDataPackets
        w.extend_from_slice(&2u16.to_be_bytes()); // totalParityPackets
        w.extend_from_slice(&1u16.to_be_bytes()); // receivedDataPackets
        w.extend_from_slice(&2u16.to_be_bytes()); // receivedParityPackets
        w.push(66); // fecPercentage
        w.push(0); // multiFecBlockIndex
        w.push(1); // multiFecBlockCount
        assert_eq!(w.len(), FecStatus::WIRE_LEN);

        let s = FecStatus::parse(&w).expect("well-formed status must parse");
        assert_eq!(s.frame_index, 1234);
        assert_eq!((s.received_data, s.total_data), (1, 3));
        assert_eq!((s.received_parity, s.total_parity), (2, 2));
        assert_eq!(s.fec_percentage, 66);
        assert_eq!(s.block_count, 1);
        assert!(!s.nothing_missing(), "two data shards short is not complete");

        assert!(FecStatus::parse(&w[..20]).is_none(), "a short status must be rejected");
    }

    /// A frame the client reports as damaged while acknowledging every shard
    /// we sent cannot be a network loss, and the tally has to separate the two
    /// or the diagnosis points at the wrong layer.
    #[test]
    fn tally_separates_lost_shards_from_host_side_damage() {
        let status = |rx_data: u16, rx_parity: u16| {
            let mut w = Vec::new();
            w.extend_from_slice(&7u32.to_be_bytes());
            for v in [10u16, 10, 0, 3, 2, rx_data, rx_parity] {
                w.extend_from_slice(&v.to_be_bytes());
            }
            w.extend_from_slice(&[66, 0, 1]);
            w
        };

        let mut r = FecReports::default();
        r.record(&status(1, 2)); // two data shards never arrived
        r.record(&status(3, 2)); // everything arrived, still damaged

        assert_eq!(r.count, 2);
        assert_eq!(r.lost_shards, 2);
        assert_eq!(r.complete_but_damaged, 1);
    }
}
