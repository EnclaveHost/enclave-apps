//! Explicit ownership of the simple-framebuffer game layer.
//!
//! Byte registers at 0x10007000: signature at 0, command at 4, then
//! little-endian x/y/width/height at 8/12/16/20. Command 1 publishes the
//! prepared rectangle and renews its lease; command 2 only renews a static
//! frame, and command 0 hides it immediately.
//! This is transient presentation state: a restored guest republishes it.

use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering};

const BASE: u64 = 0x10007000;
const LEASE: Duration = Duration::from_secs(2);
// Shared across emulator instances so a viewer switching machines cannot
// confuse two machines' first completed frames.
static NEXT_FRAME: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Legacy,
    Hidden,
    Active(u32, u32, u32, u32),
}

pub struct Overlay {
    pending: [u8; 16],
    state: State,
    renewed: Option<Instant>,
    pixels: Vec<u8>,
    pixels_rect: (u32, u32, u32, u32),
    frame_id: u64,
}

impl Overlay {
    pub fn new() -> Self {
        Self { pending: [0; 16], state: State::Legacy, renewed: None,
            pixels: Vec::new(), pixels_rect: (0, 0, 0, 0), frame_id: 0 }
    }

    pub fn load(&self, address: u64) -> u8 {
        match address.wrapping_sub(BASE) {
            offset @ 0..=3 => b"RBXO"[offset as usize],
            _ => 0,
        }
    }

    /// True requests a snapshot of the completed pixels before guest
    /// execution resumes. Presentation never reads a half-painted frame.
    pub fn store(&mut self, address: u64, value: u8) -> bool {
        match address.wrapping_sub(BASE) {
            4 => {
                let word = |i| u32::from_le_bytes([
                    self.pending[i], self.pending[i + 1],
                    self.pending[i + 2], self.pending[i + 3],
                ]);
                let (x, y, w, h) = (word(0), word(4), word(8), word(12));
                let next = if (value == 1 || value == 2) && w != 0 && h != 0 {
                    State::Active(x, y, w, h)
                } else {
                    State::Hidden
                };
                let capture = matches!(next, State::Active(..))
                    && (value == 1 || next != self.state || self.pixels.is_empty());
                self.state = next;
                self.renewed = Some(Instant::now());
                return capture;
            }
            offset @ 8..=23 => self.pending[(offset - 8) as usize] = value,
            _ => {}
        }
        false
    }

    #[inline(never)]
    pub fn capture(&mut self, stride: u64, mut read: impl FnMut(u64, &mut [u8])) {
        let State::Active(x, y, w, h) = self.state else { return };
        // The DTB reserves 8 MiB at fb0. Untrusted registers cannot allocate
        // beyond that window or make the copy address wrap.
        const MAX_BYTES: u64 = 0x80_0000;
        let stride = stride.max(4).min(MAX_BYTES);
        let (sw, sh) = (stride / 4, MAX_BYTES / stride);
        if x as u64 >= sw || y as u64 >= sh {
            self.state = State::Hidden;
            self.pixels.clear();
            return;
        }
        let w = (w as u64).min(sw - x as u64);
        let h = (h as u64).min(sh - y as u64);
        let row_bytes = (w * 4) as usize;
        self.pixels.resize(row_bytes * h as usize, 0);
        if x == 0 && w * 4 == stride {
            read(y as u64 * stride, &mut self.pixels);
        } else {
            for row in 0..h as usize {
                read((y as u64 + row as u64) * stride + x as u64 * 4,
                    &mut self.pixels[row * row_bytes..(row + 1) * row_bytes]);
            }
        }
        self.pixels_rect = (x, y, w as u32, h as u32);
        self.frame_id = NEXT_FRAME.fetch_add(1, Ordering::Relaxed);
    }

    /// Static lease renewals preserve this identity. Hiding, expiry, and an
    /// invalid rectangle expose no frame, so the desktop is scanned again.
    pub fn frame_id(&self) -> Option<u64> {
        self.frame().map(|_| self.frame_id)
    }

    pub fn frame(&self) -> Option<(u32, u32, u32, u32, &[u8])> {
        if !matches!(self.state(), State::Active(..)) || self.pixels.is_empty() {
            return None;
        }
        let (x, y, w, h) = self.pixels_rect;
        Some((x, y, w, h, &self.pixels))
    }

    pub fn state(&self) -> State {
        self.state_at(Instant::now())
    }

    fn state_at(&self, now: Instant) -> State {
        if let (State::Active(..), Some(renewed)) = (self.state, self.renewed) {
            if now.saturating_duration_since(renewed) > LEASE {
                return State::Hidden;
            }
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(d: &mut Overlay, values: [u32; 4]) {
        for (i, value) in values.iter().enumerate() {
            for (j, byte) in value.to_le_bytes().iter().enumerate() {
                d.store(BASE + 8 + (i * 4 + j) as u64, *byte);
            }
        }
    }

    #[test]
    fn publishes_fullscreen_atomically_and_expires_without_legacy_fallback() {
        let mut d = Overlay::new();
        assert_eq!(d.state(), State::Legacy);
        assert_eq!((0..4).map(|i| d.load(BASE + i)).collect::<Vec<_>>(), b"RBXO");
        rect(&mut d, [0, 0, 960, 600]);
        assert_eq!(d.state(), State::Legacy);
        d.store(BASE + 4, 1);
        assert_eq!(d.state(), State::Active(0, 0, 960, 600));
        rect(&mut d, [100, 50, 320, 200]);
        assert_eq!(d.state(), State::Active(0, 0, 960, 600));
        d.store(BASE + 4, 1);
        assert_eq!(d.state(), State::Active(100, 50, 320, 200));
        assert_eq!(d.state_at(d.renewed.unwrap() + LEASE + Duration::from_millis(1)), State::Hidden);
        d.store(BASE + 4, 1);
        assert_eq!(d.state(), State::Active(100, 50, 320, 200));
        d.store(BASE + 4, 0);
        assert_eq!(d.state(), State::Hidden);
        rect(&mut d, [0, 0, 0, 600]);
        d.store(BASE + 4, 1);
        assert_eq!(d.state(), State::Hidden);
    }

    #[test]
    fn completed_frame_identity_tracks_pixels_and_machine_ownership() {
        let mut d = Overlay::new();
        assert_eq!(d.frame_id(), None);
        rect(&mut d, [0, 0, 2, 2]);
        assert!(d.store(BASE + 4, 1));
        d.capture(8, |_, out| out.fill(7));
        let first = d.frame_id().unwrap();
        assert!(!d.store(BASE + 4, 2));
        assert_eq!(d.frame_id(), Some(first));
        assert!(d.store(BASE + 4, 1));
        d.capture(8, |_, out| out.fill(8));
        assert_ne!(d.frame_id(), Some(first));
        let second = d.frame_id().unwrap();
        let mut other = Overlay::new();
        rect(&mut other, [0, 0, 2, 2]);
        other.store(BASE + 4, 1);
        other.capture(8, |_, out| out.fill(8));
        assert_ne!(other.frame_id(), Some(second));
        d.renewed = Some(Instant::now() - LEASE - Duration::from_millis(1));
        assert_eq!(d.frame_id(), None);
        d.store(BASE + 4, 0);
        assert_eq!(d.frame_id(), None);
        rect(&mut d, [u32::MAX, 0, 2, 2]);
        d.store(BASE + 4, 1);
        d.capture(8, |_, _| panic!("invalid geometry must not read pixels"));
        assert_eq!(d.frame_id(), None);
    }
}
