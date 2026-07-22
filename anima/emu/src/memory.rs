/// Emulates main memory.
pub struct Memory {
	/// Memory content
	data: Vec<u64>
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
	pub fn init(&mut self, capacity: u64) {
		for _i in 0..((capacity + 7) / 8) {
			self.data.push(0);
		}
	}
	
	/// Reads a byte from memory.
	///
	/// # Arguments
	/// * `address`
	pub fn read_byte(&self, address: u64) -> u8 {
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		(self.data[index] >> pos) as u8
	}

	/// Reads two bytes from memory.
	///
	/// # Arguments
	/// * `address`
	pub fn read_halfword(&self, address: u64) -> u16 {
		// anima patch: any alignment reads from at most two cells (the
		// misaligned fallback was a per-byte loop)
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		match pos <= 48 {
			true => (self.data[index] >> pos) as u16,
			false => ((self.data[index] >> pos) | (self.data[index + 1] << (64 - pos))) as u16
		}
	}

	/// Reads four bytes from memory.
	///
	/// # Arguments
	/// * `address`
	pub fn read_word(&self, address: u64) -> u32 {
		// anima patch: any alignment reads from at most two cells. This is
		// hot: compressed instructions put half of all 4-byte fetches at
		// address % 4 == 2, which used to take the per-byte loop.
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		match pos <= 32 {
			true => (self.data[index] >> pos) as u32,
			false => ((self.data[index] >> pos) | (self.data[index + 1] << (64 - pos))) as u32
		}
	}

	/// Reads eight bytes from memory.
	///
	/// # Arguments
	/// * `address`
	pub fn read_doubleword(&self, address: u64) -> u64 {
		// anima patch: any alignment reads from at most two cells. Also
		// fixes an upstream bug: the 4-aligned path shifted the high word
		// by 4 instead of 32, corrupting misaligned doubleword loads.
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		match pos == 0 {
			true => self.data[index],
			false => (self.data[index] >> pos) | (self.data[index + 1] << (64 - pos))
		}
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

	/// Writes a byte to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn write_byte(&mut self, address: u64, value: u8) {
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		self.data[index] = (self.data[index] & !(0xff << pos)) | ((value as u64) << pos);
	}

	/// Writes two bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn write_halfword(&mut self, address: u64, value: u16) {
		// anima patch: any alignment writes at most two cells
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		self.data[index] = (self.data[index] & !(0xffffu64 << pos)) | ((value as u64) << pos);
		if pos > 48 {
			let shift = 64 - pos;
			self.data[index + 1] =
				(self.data[index + 1] & !(0xffffu64 >> shift)) | ((value as u64) >> shift);
		}
	}

	/// Writes four bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn write_word(&mut self, address: u64, value: u32) {
		// anima patch: any alignment writes at most two cells
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		self.data[index] = (self.data[index] & !(0xffffffffu64 << pos)) | ((value as u64) << pos);
		if pos > 32 {
			let shift = 64 - pos;
			self.data[index + 1] =
				(self.data[index + 1] & !(0xffffffffu64 >> shift)) | ((value as u64) >> shift);
		}
	}

	/// Writes eight bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn write_doubleword(&mut self, address: u64, value: u64) {
		// anima patch: any alignment writes at most two cells
		let index = (address >> 3) as usize;
		let pos = ((address % 8) as u64) * 8;
		match pos == 0 {
			true => self.data[index] = value,
			false => {
				let shift = 64 - pos;
				self.data[index] = (self.data[index] & !(u64::MAX << pos)) | (value << pos);
				self.data[index + 1] =
					(self.data[index + 1] & !(u64::MAX >> shift)) | (value >> shift);
			}
		}
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

	/// Check if the address is valid memory address
	///
	/// # Arguments
	/// * `address`
	pub fn validate_address(&self, address: u64) -> bool {
		return (address as usize) < self.data.len()
	}
}