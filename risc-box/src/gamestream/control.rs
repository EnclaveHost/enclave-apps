//! Control-channel messages: what the client asks for over ENet.
//!
//! Ported from the native bridge. The framing and the AES-GCM sealing are
//! unchanged — they have to be, the client is moonlight-common-c — and the two
//! things that differ are where the message comes from and where input goes.
//!
//! The bridge read ENet on its own thread and POSTed input to the app over
//! HTTP. In-guest there is no thread and no HTTP hop: `host.rs` pumps ENet from
//! the app's turn and hands messages here, and input lands in a queue the app
//! drains straight into the emulated machine's input device.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};

use crate::gamestream::crypto;
use crate::gamestream::session::Session;

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

/// Input events waiting for the app to inject them into the machine.
///
/// Bounded: a client that floods input must not grow this without limit, and
/// stale input is worthless anyway — dropping the oldest keeps the newest,
/// which is what a person at the far end actually wants.
const INPUT_QUEUE_MAX: usize = 256;

fn input_queue() -> &'static Mutex<VecDeque<Vec<u8>>> {
    static Q: OnceLock<Mutex<VecDeque<Vec<u8>>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Take everything queued. Called by the app's turn, which owns the emulator.
pub fn take_input() -> Vec<Vec<u8>> {
    let mut q = input_queue().lock().unwrap();
    q.drain(..).collect()
}

fn queue_input(payload: &[u8]) {
    let mut q = input_queue().lock().unwrap();
    if q.len() >= INPUT_QUEUE_MAX {
        q.pop_front();
    }
    q.push_back(payload.to_vec());
}

/// Unwrap one sealed control message. `None` for anything that fails
/// authentication — a bad tag is a forged or corrupt packet, never a message.
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

// --- input translation ---------------------------------------------------
//
// Ported verbatim from the bridge: Moonlight's wire events in, the app's own
// /hid JSON out. Kept as-is deliberately -- the virtual-key map and the
// high-resolution scroll arithmetic were derived against a real client, and
// this is the layer where a subtle mistake reads as "the mouse is slightly
// wrong" rather than as an error.

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
        _ => None,
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

/// One control message from the client.
pub fn on_message(session: &Session, _channel: u8, data: &[u8]) {
    let Some((msg_type, payload)) = unseal(session, data) else {
        // Not worth a line per packet: a client that cannot seal correctly
        // sends a lot of them.
        return;
    };
    match msg_type {
        CTRL_INPUT_DATA => {
            // Translate on arrival: the app's turn drains this queue and
            // injects it, and it should not be doing wire-format work there.
            if let Some(json) = input_event_json(session, &payload) {
                queue_input(json.as_bytes());
            }
        }
        CTRL_REQUEST_IDR => {
            eprintln!("[control] IDR requested");
            session.request_idr();
            // The hardware encoder can honour this on the very next frame,
            // which the ffmpeg-through-a-pipe path it replaces could not.
            crate::worker::force_key();
        }
        CTRL_INVALIDATE_REF_FRAMES => {
            session.request_idr();
            crate::worker::force_key();
        }
        CTRL_TERMINATION => {
            eprintln!("[control] client asked to end the session");
            session.stop();
        }
        // Keepalives and telemetry: nothing to do, but receiving them is what
        // keeps the peer alive.
        CTRL_PERIODIC_PING
        | CTRL_LOSS_STATS
        | CTRL_START_B
        | CTRL_LTR_ACK
        | CTRL_FRAME_FEC_STATUS => {}
        other => eprintln!("[control] unhandled message type {other:#06x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seal a message the way the client does, so unseal is tested against the
    /// format it will actually meet rather than against itself.
    fn seal(key: &[u8], seq: u32, msg_type: u16, body: &[u8]) -> Vec<u8> {
        let iv = crypto::control_iv(seq, false);
        let mut plaintext = msg_type.to_le_bytes().to_vec();
        plaintext.extend_from_slice(&[0, 0]); // inner length/padding
        plaintext.extend_from_slice(body);
        let (tag, ct) = crypto::gcm_encrypt(key, &iv, &plaintext);

        let mut out = CTRL_ENCRYPTED.to_le_bytes().to_vec();
        let length = 4 + 16 + ct.len();
        out.extend_from_slice(&(length as u16).to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&ct);
        out
    }

    fn session_with_key(key: Vec<u8>) -> Session {
        Session::new(1, key, 0x2233_4455, "0123456789abcdef".into(), 0xDEAD_BEEF, 0)
    }

    #[test]
    fn a_client_sealed_message_unseals() {
        let key = vec![0x11u8; 16];
        let s = session_with_key(key.clone());
        let msg = seal(&key, 7, CTRL_REQUEST_IDR, &[]);
        let (t, _) = unseal(&s, &msg).expect("a correctly sealed message must unseal");
        assert_eq!(t, CTRL_REQUEST_IDR);
    }

    /// A forged packet must not become a message. This is the property that
    /// makes it safe to act on control input at all.
    #[test]
    fn a_tampered_message_is_rejected() {
        let key = vec![0x11u8; 16];
        let s = session_with_key(key.clone());
        let mut msg = seal(&key, 7, CTRL_REQUEST_IDR, &[]);
        let last = msg.len() - 1;
        msg[last] ^= 0x01;
        assert!(unseal(&s, &msg).is_none(), "a flipped ciphertext bit must fail the tag");
    }

    /// Truncated and plaintext frames must be refused rather than indexed into.
    #[test]
    fn malformed_frames_are_refused() {
        let s = session_with_key(vec![0x11u8; 16]);
        assert!(unseal(&s, &[]).is_none());
        assert!(unseal(&s, &[1, 0, 0, 0]).is_none());
        // Right header, impossible length.
        let mut short = CTRL_ENCRYPTED.to_le_bytes().to_vec();
        short.extend_from_slice(&4u16.to_le_bytes());
        short.extend_from_slice(&[0; 8]);
        assert!(unseal(&s, &short).is_none());
    }

    /// A flood of input must not grow without bound, and must keep the NEWEST
    /// events -- stale input is worthless to whoever is holding the mouse.
    #[test]
    fn the_input_queue_is_bounded_and_keeps_the_newest() {
        let _ = take_input(); // start clean
        for i in 0..(INPUT_QUEUE_MAX + 50) {
            queue_input(&[i as u8]);
        }
        let got = take_input();
        assert_eq!(got.len(), INPUT_QUEUE_MAX, "the queue must be bounded");
        assert_eq!(
            got.last().unwrap()[0],
            (INPUT_QUEUE_MAX + 49) as u8,
            "the newest event must survive"
        );
        assert!(take_input().is_empty(), "taking must drain");
    }
}
