use cpu::{MIP_MSIP, MIP_MTIP};

/// Emulates CLINT known as Timer. Refer to the [specification](https://sifive.cdn.prismic.io/sifive%2Fc89f6e5a-cf9e-44c3-a3db-04420702dcc1_sifive+e31+manual+v19.08.pdf)
/// for the detail.
pub struct Clint {
	clock: u64,
	// risc-box patch (SMP): one msip/mtimecmp per hart (the SiFive CLINT layout: msip[h] at +4h, mtimecmp[h]
	// at 0x4000 + 8h); mtime is shared
	msip: Vec<u32>,
	mtimecmp: Vec<u64>,
	mtime: u64,
	// risc-box patch: see `set_wall_clock`.
	wall: Option<Wall>
}

/// risc-box patch: state for wall-clock mtime.
struct Wall {
	start: std::time::Instant,
	// risc-box patch (snapshot): mtime at `start`. Zero on a cold boot; a
	// restored machine resumes from the snapshot's mtime rather than from
	// zero, because the guest's armed timer deadlines (mtimecmp) are
	// absolute and a clock that jumped backwards would leave it waiting
	// out the whole snapshot's uptime before its next tick.
	base: u64,
	since_sample: u64
}

/// How many retired instructions between host-clock samples. The host clock is
/// a syscall-ish cost, so it cannot be read per instruction; at any speed this
/// emulator runs, 4096 instructions is tens of microseconds, far finer than the
/// 10 ms the guest kernel's tick and the 28.6 ms a DOOM tic care about.
const WALL_SAMPLE_INSNS: u64 = 4096;

impl Clint {
	/// Creates a new `Clint`
	pub fn new() -> Self {
		Clint {
			clock: 0,
			msip: vec![0],
			mtimecmp: vec![0],
			mtime: 0, // @TODO: Should be bound to csr time register
			wall: None
		}
	}

	/// risc-box patch: drive `mtime` from the host's monotonic clock instead of
	/// from retired instructions.
	///
	/// Instruction-driven time is right for a benchmark — it makes a boot
	/// deterministic — but it is a lie to anything that watches the clock to
	/// pace itself. The DTB advertises a 10 MHz timebase, so a guest believes
	/// one second has passed every 10M instructions; an emulator retiring
	/// 120M/s therefore hands the guest twelve seconds per real second, and a
	/// game paced off that clock runs at twelve times speed while still
	/// *looking* correct from inside. Under this mode the guest's second is a
	/// real second: `sleep 1` takes a second, the kernel takes HZ timer
	/// interrupts per real second rather than twelve times HZ, and a frame rate
	/// measured in the guest means what it says.
	/// risc-box patch: the machine's monotonic 10 MHz tick — the same clock
	/// the guest reads. Devices that must run at real speed (the sound card's
	/// playback rate) pace off this rather than off retired instructions.
	pub fn mtime(&self) -> u64 {
		self.mtime
	}

	/// risc-box patch (SMP): how many harts this CLINT serves.
	pub fn set_harts(&mut self, n: usize) {
		self.msip.resize(n.max(1), 0);
		self.mtimecmp.resize(n.max(1), 0);
	}

	pub fn set_wall_clock(&mut self, on: bool) {
		self.wall = match on {
			true => Some(Wall {
				start: std::time::Instant::now(),
				base: 0,
				// sample on the very first tick so the guest never sees zero
				since_sample: WALL_SAMPLE_INSNS
			}),
			false => None
		};
	}

	/// Runs one cycle. `Clint` can raise interrupt. If it does it rises a certain bit
	/// depending on interrupt type of CPU `mip` register.
	///
	/// # Arguments
	/// * `mip` CPU `mip` register. It can be updated if interrupt occurs.
	// risc-box patch: `n` instructions have retired since the last call. mtime
	// advances by exactly that, so the guest's clock keeps the same rate it
	// had when this ran every instruction — only its granularity changes.
	pub fn tick(&mut self, n: u64, mip: &mut u64, raised: &[std::sync::atomic::AtomicU64]) {
		self.clock = self.clock.wrapping_add(n);
		match self.wall {
			// risc-box patch: wall-clock mode. mtime is the host's monotonic
			// clock at 10 MHz (the timebase the DTB advertises), resampled
			// every WALL_SAMPLE_INSNS; between samples it holds still, which
			// is monotonic and finer-grained than any deadline the guest sets.
			Some(ref mut w) => {
				w.since_sample = w.since_sample.wrapping_add(n);
				if w.since_sample >= WALL_SAMPLE_INSNS {
					w.since_sample = 0;
					self.mtime = w.base.wrapping_add((w.start.elapsed().as_nanos() as u64) / 100);
				}
			},
			None => self.mtime = self.mtime.wrapping_add(n)
		}

		// hart 0's bits go straight into the calling hart's mip, as they always have; the other harts' are
		// raised for them to fold in at their own next service (see Cpu::run)
		for h in 0..self.msip.len() {
			let mut bits = 0;
			if (self.msip[h] & 1) != 0 {
				bits |= MIP_MSIP;
			}
			if self.mtimecmp[h] > 0 && self.mtime >= self.mtimecmp[h] {
				bits |= MIP_MTIP;
			}
			if bits != 0 {
				match h {
					0 => *mip |= bits,
					_ => { raised[h].fetch_or(bits, std::sync::atomic::Ordering::AcqRel); }
				}
			}
		}
	}

	/// Loads register content.
	///
	/// # Arguments
	/// * `address`
	pub fn load(&self, address: u64) -> u8 {
		//println!("CLINT Load AD:{:X}", address);
		match address {
			// MSIP registers, 4 bytes per hart
			0x02000000..=0x02003fff => {
				let h = ((address - 0x02000000) >> 2) as usize;
				match self.msip.get(h) {
					Some(v) => (v >> ((address & 3) * 8)) as u8,
					None => 0
				}
			},
			// MTIMECMP registers, 8 bytes per hart
			0x02004000..=0x0200bff7 => {
				let h = ((address - 0x02004000) >> 3) as usize;
				match self.mtimecmp.get(h) {
					Some(v) => (v >> ((address & 7) * 8)) as u8,
					None => 0
				}
			},
			0x0200bff8 => {
				self.mtime as u8
			},
			0x0200bff9 => {
				(self.mtime >> 8) as u8
			},
			0x0200bffa => {
				(self.mtime >> 16) as u8
			},
			0x0200bffb => {
				(self.mtime >> 24) as u8
			},
			0x0200bffc => {
				(self.mtime >> 32) as u8
			},
			0x0200bffd => {
				(self.mtime >> 40) as u8
			},
			0x0200bffe => {
				(self.mtime >> 48) as u8
			},
			0x0200bfff => {
				(self.mtime >> 56) as u8
			},
			_ => 0,
		}
	}

	/// Stores register content.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn store(&mut self, address: u64, value: u8) {
		//println!("CLINT Store AD:{:X} VAL:{:X}", address, value);
		match address {
			// MSIP registers, 4 bytes per hart. Upper 31 bits are hardwired to zero.
			0x02000000..=0x02003fff => {
				let h = ((address - 0x02000000) >> 2) as usize;
				if (address & 3) == 0 && h < self.msip.len() {
					self.msip[h] = (self.msip[h] & !0x1) | ((value & 1) as u32);
				}
			},
			// MTIMECMP registers, 8 bytes per hart
			0x02004000..=0x0200bff7 => {
				let h = ((address - 0x02004000) >> 3) as usize;
				if h < self.mtimecmp.len() {
					let pos = (address & 7) * 8;
					self.mtimecmp[h] = (self.mtimecmp[h] & !(0xff << pos)) | ((value as u64) << pos);
				}
			},
			// MTIME registers 8 bytes
			0x0200bff8 => {
				self.mtime = (self.mtime & !0xff) | (value as u64);
			},
			0x0200bff9 => {
				self.mtime = (self.mtime & !(0xff << 8)) | ((value as u64) << 8);
			},
			0x0200bffa => {
				self.mtime = (self.mtime & !(0xff << 16)) | ((value as u64) << 16);
			},
			0x0200bffb => {
				self.mtime = (self.mtime & !(0xff << 24)) | ((value as u64) << 24);
			},
			0x0200bffc => {
				self.mtime = (self.mtime & !(0xff << 32)) | ((value as u64) << 32);
			},
			0x0200bffd => {
				self.mtime = (self.mtime & !(0xff << 40)) | ((value as u64) << 40);
			},
			0x0200bffe => {
				self.mtime = (self.mtime & !(0xff << 48)) | ((value as u64) << 48);
			},
			0x0200bfff => {
				self.mtime = (self.mtime & !(0xff << 56)) | ((value as u64) << 56);
			},
			_ => {}
		};
	}

	/// Reads `mtime` register content
	pub fn read_mtime(&self) -> u64 {
		self.mtime
	}

	/// Writes to `mtime` register content
	pub fn write_mtime(&mut self, value: u64) {
		self.mtime = value;
	}

	/// risc-box patch (diagnosis): the armed deadline, and whether mtime is being
	/// driven by the host clock or by retired instructions. A guest parked in WFI
	/// wakes on `mtime >= mtimecmp` and on nothing else, so these two numbers are
	/// the difference between "idle" and "never coming back".
	pub fn read_mtimecmp(&self) -> u64 {
		self.mtimecmp[0]
	}
	pub fn is_wall(&self) -> bool {
		self.wall.is_some()
	}
}

// risc-box patch (snapshot): see src/snapshot.rs.
use snapshot::{De, Ser};

impl Clint {
	pub fn snapshot(&self, w: &mut Ser) {
		w.u64(self.clock);
		w.u32(self.msip[0]);
		w.u64(self.mtimecmp[0]);
		w.u64(self.mtime);
		w.bool(self.wall.is_some());
		// risc-box patch (SMP): the other harts' registers follow; a one-hart machine writes exactly the old format
		for h in 1..self.msip.len() {
			w.u32(self.msip[h]);
			w.u64(self.mtimecmp[h]);
		}
	}

	/// The clock source (instruction-driven or wall) stays whatever THIS
	/// machine was configured with; only the count carries over. In wall
	/// mode the host clock is re-based so mtime continues from the
	/// snapshot's value on the next sample.
	pub fn restore(&mut self, r: &mut De) -> Result<(), String> {
		self.clock = r.u64()?;
		self.msip[0] = r.u32()?;
		self.mtimecmp[0] = r.u64()?;
		self.mtime = r.u64()?;
		let _was_wall = r.bool()?;
		for h in 1..self.msip.len() {
			self.msip[h] = r.u32()?;
			self.mtimecmp[h] = r.u64()?;
		}
		if self.wall.is_some() {
			self.wall = Some(Wall {
				start: std::time::Instant::now(),
				base: self.mtime,
				since_sample: WALL_SAMPLE_INSNS
			});
		}
		Ok(())
	}
}
