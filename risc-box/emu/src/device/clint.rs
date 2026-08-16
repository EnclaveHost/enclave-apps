use cpu::{MIP_MSIP, MIP_MTIP};

/// Emulates CLINT known as Timer. Refer to the [specification](https://sifive.cdn.prismic.io/sifive%2Fc89f6e5a-cf9e-44c3-a3db-04420702dcc1_sifive+e31+manual+v19.08.pdf)
/// for the detail.
pub struct Clint {
	clock: u64,
	msip: u32,
	mtimecmp: u64,
	mtime: u64,
	// risc-box patch: see `set_wall_clock`.
	wall: Option<Wall>
}

/// risc-box patch: state for wall-clock mtime.
struct Wall {
	start: std::time::Instant,
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
			msip: 0,
			mtimecmp: 0,
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
	pub fn set_wall_clock(&mut self, on: bool) {
		self.wall = match on {
			true => Some(Wall {
				start: std::time::Instant::now(),
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
	pub fn tick(&mut self, n: u64, mip: &mut u64) {
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
					self.mtime = (w.start.elapsed().as_nanos() as u64) / 100;
				}
			},
			None => self.mtime = self.mtime.wrapping_add(n)
		}

		if (self.msip & 1) != 0 {
			*mip |= MIP_MSIP;
		}

		if self.mtimecmp > 0 && self.mtime >= self.mtimecmp {
			*mip |= MIP_MTIP;
		}
	}

	/// Loads register content.
	///
	/// # Arguments
	/// * `address`
	pub fn load(&self, address: u64) -> u8 {
		//println!("CLINT Load AD:{:X}", address);
		match address {
			// MSIP register 4 bytes
			0x02000000 => {
				(self.msip & 0xff) as u8
			},
			0x02000001 => {
				((self.msip >> 8) & 0xff) as u8
			},
			0x02000002 => {
				((self.msip >> 16) & 0xff) as u8
			},
			0x02000003 => {
				((self.msip >> 24) & 0xff) as u8
			},
			// MTIMECMP Registers 8 bytes
			0x02004000 => {
				self.mtimecmp as u8
			},
			0x02004001 => {
				(self.mtimecmp >> 8) as u8
			},
			0x02004002 => {
				(self.mtimecmp >> 16) as u8
			},
			0x02004003 => {
				(self.mtimecmp >> 24) as u8
			},
			0x02004004 => {
				(self.mtimecmp >> 32) as u8
			},
			0x02004005 => {
				(self.mtimecmp >> 40) as u8
			},
			0x02004006 => {
				(self.mtimecmp >> 48) as u8
			},
			0x02004007 => {
				(self.mtimecmp >> 56) as u8
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
			// MSIP register 4 bytes. Upper 31 bits are hardwired to zero.
			0x02000000 => {
				self.msip = (self.msip & !0x1) | ((value & 1) as u32);
			},
			// MTIMECMP Registers 8 bytes
			0x02004000 => {
				self.mtimecmp = (self.mtimecmp & !0xff) | (value as u64);
			},
			0x02004001 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 8)) | ((value as u64) << 8);
			},
			0x02004002 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 16)) | ((value as u64) << 16);
			},
			0x02004003 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 24)) | ((value as u64) << 24);
			},
			0x02004004 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 32)) | ((value as u64) << 32);
			},
			0x02004005 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 40)) | ((value as u64) << 40);
			},
			0x02004006 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 48)) | ((value as u64) << 48);
			},
			0x02004007 => {
				self.mtimecmp = (self.mtimecmp & !(0xff << 56)) | ((value as u64) << 56);
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
}
