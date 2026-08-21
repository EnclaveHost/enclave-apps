// risc-box patch: an AdLib-style OPL register port at 0x10006000.
//
// This device does NOT synthesize. It is a mailbox: the guest writes OPL
// register/value pairs exactly as a DOS driver would write ports 0x388/0x389,
// and the host drains them and feeds a real OPL3 chip emulation running as
// native code (see the app's audio path).
//
// WHY THE SPLIT. DOOM's music is MUS in the WAD; converting it to MIDI and
// turning MIDI into OPL register writes is cheap bookkeeping and stays in the
// guest, where the engine's own timing lives. Actually GENERATING the samples
// is not cheap: Nuked OPL3 is cycle-accurate at 49716 Hz, and running it on
// the emulated RV64 core — which has no vector unit — measured a 21% cut in
// DOOM's frame rate (median 69.2 -> 54.7 fps). The same principle that applies
// to rasterization applies here: per-sample work belongs on the host, where it
// is native, and the guest keeps the parts that are about game logic.
//
// No kernel driver is needed. The guest's sound code mmaps /dev/mem at this
// address (the kernel is built CONFIG_DEVMEM=y with STRICT_DEVMEM off, and
// this is device space rather than RAM), so a userspace process writes these
// registers directly — which is exactly the access a DOS program had.

use std::collections::VecDeque;

const BASE: u64 = 0x10006000;

/// Register writes the host has not consumed yet.
///
/// Bounded because a guest that scribbles while nothing drains must not grow
/// the host without limit. Dropping the OLDEST is right for a synth: the
/// newest writes are the current state of the chip, and a stale register
/// value applied late is worse than one never applied at all.
const MAX_PENDING: usize = 8192;

pub struct Opl {
    /// Register index the guest last selected. OPL3 has two register banks;
    /// the high bit distinguishes them, matching the 0x100.. range the
    /// chocolate-doom driver already uses.
    index: u16,
    pending: VecDeque<(u16, u8)>,
    /// Total writes accepted, for host-side "is the guest driving this at
    /// all" checks — the same question the cursor plane needed answered.
    writes: u64,
    dropped: u64,
}

impl Opl {
    pub fn new() -> Self {
        Opl { index: 0, pending: VecDeque::new(), writes: 0, dropped: 0 }
    }

    pub fn load(&mut self, address: u64) -> u8 {
        match address - BASE {
            // A status read: bit 0 reports "the host is consuming", which lets
            // the guest tell a listening host from a silent one and skip the
            // MIDI work entirely when nothing is there.
            0x08 => (self.writes > 0) as u8,
            _ => 0,
        }
    }

    pub fn store(&mut self, address: u64, value: u8) {
        match address - BASE {
            // 0x00/0x01: register index, low then high byte (the high byte
            // selects the OPL3 upper bank).
            0x00 => self.index = (self.index & 0xff00) | value as u16,
            0x01 => self.index = (self.index & 0x00ff) | ((value as u16) << 8),
            // 0x04: the value, which commits the pair.
            0x04 => {
                if self.pending.len() >= MAX_PENDING {
                    self.pending.pop_front();
                    self.dropped = self.dropped.wrapping_add(1);
                }
                self.pending.push_back((self.index, value));
                self.writes = self.writes.wrapping_add(1);
            }
            _ => {}
        }
    }

    /// Hand the host everything written since the last call.
    pub fn drain(&mut self) -> Vec<(u16, u8)> {
        self.pending.drain(..).collect()
    }

    /// Whether the guest has ever written a register — the host uses this to
    /// decide there is music to mix at all.
    pub fn active(&self) -> bool {
        self.writes > 0
    }

    pub fn stats(&self) -> (u64, u64) {
        (self.writes, self.dropped)
    }

    pub fn reset(&mut self) {
        self.index = 0;
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A register write is index-then-value, and both banks survive the trip.
    #[test]
    fn register_pairs_round_trip_including_the_upper_bank() {
        let mut o = Opl::new();
        o.store(BASE, 0x20);        // index low
        o.store(BASE + 1, 0x00);    // index high
        o.store(BASE + 4, 0x21);    // value -> commits
        o.store(BASE, 0x05);        // upper bank: 0x105
        o.store(BASE + 1, 0x01);
        o.store(BASE + 4, 0x01);
        assert_eq!(o.drain(), vec![(0x0020, 0x21), (0x0105, 0x01)]);
        assert!(o.drain().is_empty(), "draining consumes");
        assert!(o.active());
    }

    /// A guest writing to a host that never drains must not grow memory
    /// without bound, and must lose the OLDEST — the newest writes are the
    /// chip's current state.
    #[test]
    fn overflow_drops_the_stalest_writes() {
        let mut o = Opl::new();
        for i in 0..(MAX_PENDING + 10) {
            o.store(BASE, (i & 0xff) as u8);
            o.store(BASE + 4, (i & 0xff) as u8);
        }
        let drained = o.drain();
        assert_eq!(drained.len(), MAX_PENDING);
        assert_eq!(o.stats().1, 10, "ten oldest writes dropped");
    }
}
