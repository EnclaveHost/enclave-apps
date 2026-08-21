//! The machine's music synth, running on the host.
//!
//! The guest writes OPL registers into the emulator's mailbox
//! (emu/src/device/opl.rs) exactly as a DOS program wrote ports 0x388/0x389.
//! Here those writes are applied to a Nuked OPL3 chip compiled as native code
//! (vendor/opl3) and the samples are mixed into the PCM the sound card
//! produced, before it goes out to listeners.
//!
//! The split is the whole point. MUS-to-MIDI conversion, GENMIDI instrument
//! handling and event scheduling are cheap bookkeeping and stay in the guest
//! where DOOM's timing lives. Generating samples is not cheap — measured, the
//! same synthesis inside the guest cost 21% of DOOM's frame rate (median 69.2
//! to 54.7 fps), because a cycle-accurate 49716 Hz chip on an emulated RV64
//! core with no vector unit is the worst case for that machine.

use riscv_emu_rust::Emulator;

extern "C" {
    fn rbx_opl_init(rate: u32);
    fn rbx_opl_write(reg: u16, value: u8);
    fn rbx_opl_generate(out: *mut i16, frames: u32);
}

pub struct Opl {
    rate: u32,
    started: bool,
    /// Scratch for generated music, reused so a mix allocates nothing.
    buf: Vec<i16>,
}

impl Opl {
    pub fn new() -> Self {
        Opl { rate: 0, started: false, buf: Vec::new() }
    }

    /// Apply whatever the guest has written and mix music into `pcm` in place.
    ///
    /// `pcm` is s16le stereo at `rate`, straight from the sound card. The
    /// synth generates exactly as many frames as the card produced, which is
    /// what keeps music in step with effects: the card is metered against real
    /// time, so matching its frame count inherits that clock for free.
    pub fn mix(&mut self, emu: &mut Emulator, pcm: &mut [u8], rate: u32, channels: u8) {
        let writes = emu.opl_take_writes();
        if !writes.is_empty() && !self.started {
            // Reset the chip at the stream's rate the first time the guest
            // actually drives it, not at boot: the card's rate is the guest's
            // to choose and is not known until it opens the stream.
            unsafe { rbx_opl_init(rate) };
            self.rate = rate;
            self.started = true;
        }
        if self.started && rate != self.rate {
            unsafe { rbx_opl_init(rate) };
            self.rate = rate;
        }
        for (reg, val) in writes {
            unsafe { rbx_opl_write(reg, val) };
        }
        if !self.started || pcm.is_empty() {
            return;
        }
        // Stereo frames in the card's stream. A mono card still gets stereo
        // music folded down, rather than no music at all.
        let bytes_per_frame = 2 * channels.max(1) as usize;
        let frames = pcm.len() / bytes_per_frame;
        if frames == 0 {
            return;
        }
        if self.buf.len() < frames * 2 {
            self.buf.resize(frames * 2, 0);
        }
        unsafe { rbx_opl_generate(self.buf.as_mut_ptr(), frames as u32) };

        for f in 0..frames {
            for c in 0..channels.max(1) as usize {
                let at = f * bytes_per_frame + c * 2;
                let sfx = i16::from_le_bytes([pcm[at], pcm[at + 1]]) as i32;
                // Halve the synth before summing: OPL output is loud beside
                // DOOM's samples, and a clipped mix sounds far worse than a
                // quiet one.
                let mus = (self.buf[f * 2 + c.min(1)] as i32) / 2;
                let sum = (sfx + mus).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let b = sum.to_le_bytes();
                pcm[at] = b[0];
                pcm[at + 1] = b[1];
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.started
    }
}
