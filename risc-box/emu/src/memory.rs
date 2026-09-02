/// Emulates main memory.
// risc-box patch: DRAM is a byte array. It was a Vec<u64> whose accessors
// assembled every read and write from shifted/masked cells (a store was a
// read-modify-write of up to two cells); to_le_bytes/from_le_bytes on a
// byte slice compile to single unaligned loads/stores on x86 and wasm,
// and the byte order the guest sees is identical (little-endian cells).
use std::convert::TryInto; // edition-2015 crate: not in the prelude

pub struct Memory {
	/// Memory content
	data: Vec<u8>
}

impl Memory {
	/// Creates a new `Memory`
	pub fn new() -> Self {
		Memory {
			data: vec![]
		}
	}

	/// Initializes memory content.
	/// This method is expected to be called only once.
	///
	/// # Arguments
	/// * `capacity`
	// risc-box patch: allocate the guest's DRAM in ONE sized allocation instead
	// of pushing it a word at a time. `push` grows by doubling, so the final
	// step holds the old buffer AND the new one — 768 MiB live to arrive at a
	// 512 MiB array — and memcpys the whole thing across. Under the SET build,
	// whose shared memory has its maximum fixed at LINK time and cannot grow
	// past it, that transient is the difference between booting and:
	//
	//     memory allocation of 536870912 bytes failed   (Mmu::init_memory)
	//
	// with a 128 MiB rootfs already resident. One sized allocation has no old
	// buffer, no copy, and no 64-million-iteration loop to walk at every boot.
	// `init` is called once on a fresh Memory (upstream's own contract, stated
	// just above), so replacing the vector is what appending to it meant.
	pub fn init(&mut self, capacity: u64) {
		// rounded up to a whole number of 8-byte cells like the old layout,
		// so edge-of-DRAM doubleword accesses stay in bounds
		self.data = vec![0; (((capacity + 7) / 8) * 8) as usize];
	}

	/// Reads a byte from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_byte(&self, address: u64) -> u8 {
		self.data[address as usize]
	}

	/// Reads two bytes from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_halfword(&self, address: u64) -> u16 {
		let a = address as usize;
		u16::from_le_bytes(self.data[a..a + 2].try_into().unwrap())
	}

	/// Reads four bytes from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_word(&self, address: u64) -> u32 {
		let a = address as usize;
		u32::from_le_bytes(self.data[a..a + 4].try_into().unwrap())
	}

	/// Reads eight bytes from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_doubleword(&self, address: u64) -> u64 {
		let a = address as usize;
		u64::from_le_bytes(self.data[a..a + 8].try_into().unwrap())
	}

	/// Reads multiple bytes from memory.
	///
	/// # Arguments
	/// * `address`
	/// * `width` up to eight
	pub fn read_bytes(&self, address: u64, width: u64) -> u64 {
		let mut data = 0 as u64;
		for i in 0..width {
			data |= (self.read_byte(address.wrapping_add(i)) as u64) << (i * 8);
		}
		data
	}

	/// risc-box patch: bulk-copies `out.len()` bytes starting at `address`
	/// into `out` — the framebuffer scanout path. With byte-backed DRAM this
	/// is a straight memcpy.
	pub fn read_range(&self, address: u64, out: &mut [u8]) {
		let a = address as usize;
		out.copy_from_slice(&self.data[a..a + out.len()]);
	}

	/// Writes a byte to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_byte(&mut self, address: u64, value: u8) {
		self.data[address as usize] = value;
	}

	/// Writes two bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_halfword(&mut self, address: u64, value: u16) {
		let a = address as usize;
		self.data[a..a + 2].copy_from_slice(&value.to_le_bytes());
	}

	/// Writes four bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_word(&mut self, address: u64, value: u32) {
		let a = address as usize;
		self.data[a..a + 4].copy_from_slice(&value.to_le_bytes());
	}

	/// Writes eight bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_doubleword(&mut self, address: u64, value: u64) {
		let a = address as usize;
		self.data[a..a + 8].copy_from_slice(&value.to_le_bytes());
	}

	/// Write multiple bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	/// * `width` up to eight
	pub fn write_bytes(&mut self, address: u64, value: u64, width: u64) {
		for i in 0..width {
			self.write_byte(address.wrapping_add(i), (value >> (i * 8)) as u8);
		}
	}

	/// risc-box patch (snapshot): the whole of DRAM, for the sparse page
	/// codec. Reads only; the write side goes through `as_mut_slice` so a
	/// restore fills pages in place with no second allocation.
	pub fn as_slice(&self) -> &[u8] {
		&self.data
	}

	pub fn as_mut_slice(&mut self) -> &mut [u8] {
		&mut self.data
	}

	/// Check if the address is valid memory address
	///
	/// # Arguments
	/// * `address`
	pub fn validate_address(&self, address: u64) -> bool {
		return (address as usize) < self.data.len()
	}
}
