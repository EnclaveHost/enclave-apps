/// Emulates main memory.
// risc-box patch: DRAM was one contiguous Vec<u8>. It is now an array of
// 64 KiB chunks in one of three states — never touched (reads as zero,
// costs nothing), SHARED with other machines (an Arc, copied on the first
// write), or OWNED by this machine. Two things fall out of that:
//
// - a machine costs what it has TOUCHED, not what it was configured with:
//   a 512 MiB guest whose kernel has used 90 MiB holds 90 MiB;
// - machines forked from one snapshot share every page they have not
//   diverged on, so N instances of one booted image cost one image plus
//   N × (what each has written since). That is what lets an app host many
//   guests inside one 4 GiB wasm32 address space.
//
// The accessors keep the byte-slice fast path the old layout had (a load is
// one chunk index, one bounds check, one unaligned read); an access that
// would cross a chunk falls back to bytes. The MMU never issues one on its
// hot path — every load/store fast path is guarded to a single 4 KiB page —
// so the fallback only ever runs for device DMA at an odd address.
use std::sync::Arc;

pub const CHUNK_SHIFT: u32 = 16;
pub const CHUNK: usize = 1 << CHUNK_SHIFT;
pub const PAGE: usize = 4096;

pub type Chunk = [u8; CHUNK];

fn zero_box() -> Box<Chunk> {
	// zeroed u8 array: initialized by definition
	unsafe { Box::<Chunk>::new_zeroed().assume_init() }
}

fn zero_arc() -> Arc<Chunk> {
	unsafe { Arc::<Chunk>::new_zeroed().assume_init() }
}

enum Slot {
	/// Never written: reads as zero. Costs nothing.
	Zero,
	/// A page set shared with a snapshot image and/or other machines.
	/// Copied into an Owned chunk on the first write (copy-on-write).
	Shared(Arc<Chunk>),
	/// This machine's own bytes.
	Owned(Box<Chunk>)
}

impl Slot {
	#[inline(always)]
	fn bytes(&self) -> Option<&Chunk> {
		match self {
			Slot::Zero => None,
			Slot::Shared(c) => Some(c),
			Slot::Owned(b) => Some(b)
		}
	}
}

/// A read-only set of pages that machines can share: what a snapshot
/// inflates to, and what a fork starts from. Built page by page (the
/// snapshot decoder writes into it), then handed to any number of
/// `Memory::adopt` calls, each of which is O(chunks) reference bumps.
#[derive(Clone)]
pub struct RamImage {
	chunks: Vec<Option<Arc<Chunk>>>,
	len: usize
}

impl RamImage {
	pub fn new(len: usize) -> Self {
		let n = (len + CHUNK - 1) / CHUNK;
		RamImage { chunks: (0..n).map(|_| None).collect(), len }
	}

	pub fn len(&self) -> usize {
		self.len
	}

	/// Store one PAGE-sized page. Only called while the image is being
	/// built, when every chunk is uniquely owned.
	pub fn write_page(&mut self, page: usize, data: &[u8]) {
		let addr = page * PAGE;
		let ci = addr >> CHUNK_SHIFT;
		let off = addr & (CHUNK - 1);
		let chunk = self.chunks[ci].get_or_insert_with(zero_arc);
		let chunk = Arc::get_mut(chunk).expect("RamImage chunk is shared while being built");
		chunk[off..off + data.len()].copy_from_slice(data);
	}

	pub fn page(&self, page: usize) -> Option<&[u8]> {
		let addr = page * PAGE;
		let c = self.chunks.get(addr >> CHUNK_SHIFT)?.as_ref()?;
		let off = addr & (CHUNK - 1);
		Some(&c[off..off + PAGE])
	}

	/// Bytes actually allocated (chunks with content), for accounting.
	pub fn footprint(&self) -> usize {
		self.chunks.iter().filter(|c| c.is_some()).count() * CHUNK
	}
}

/// A chunk of zeros every untouched slot reads from, so a read never
/// branches on the slot's state: it just follows the pointer.
static ZERO_CHUNK: Chunk = [0; CHUNK];

pub struct Memory {
	slots: Vec<Slot>,
	// The hot path. `rd[i]` points at the bytes a read of chunk i should
	// see (the owned or shared chunk, or ZERO_CHUNK); `wr[i]` points at the
	// owned chunk or is null, which sends the write down the slow path that
	// allocates or copies. Both are derived from `slots` and refreshed by
	// `set_slot`, the only place a slot changes. Boxes and Arcs never move
	// their allocation, so a pointer taken from one stays valid for exactly
	// as long as the slot holds it.
	rd: Vec<*const u8>,
	wr: Vec<*mut u8>,
	len: usize
}

// The raw pointers only ever point into allocations this struct owns (or
// shares through an Arc it holds), so moving or sharing the struct across
// threads is as sound as it is for the Vec<Slot> behind them.
unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

impl Memory {
	/// Creates a new `Memory`
	pub fn new() -> Self {
		Memory { slots: vec![], rd: vec![], wr: vec![], len: 0 }
	}

	/// Install a slot and refresh the pointer tables for it.
	fn set_slot(&mut self, ci: usize, slot: Slot) {
		let (rd, wr): (*const u8, *mut u8) = match &slot {
			Slot::Zero => (ZERO_CHUNK.as_ptr(), std::ptr::null_mut()),
			Slot::Shared(a) => (a.as_ptr(), std::ptr::null_mut()),
			Slot::Owned(b) => (b.as_ptr(), std::ptr::null_mut())
		};
		self.slots[ci] = slot;
		self.rd[ci] = rd;
		self.wr[ci] = match &mut self.slots[ci] {
			Slot::Owned(b) => b.as_mut_ptr(),
			_ => wr
		};
	}

	/// Initializes memory content.
	/// This method is expected to be called only once.
	///
	/// # Arguments
	/// * `capacity`
	// risc-box patch: nothing is allocated here. Every chunk starts Zero and
	// is materialized by the first write into it, so a 512 MiB guest costs
	// the host what it touches. (The old single Vec was the difference
	// between booting and `memory allocation of 536870912 bytes failed`
	// under the SET build; that constraint is gone with it.)
	pub fn init(&mut self, capacity: u64) {
		// rounded up to a whole number of 8-byte cells like the old layout,
		// so edge-of-DRAM doubleword accesses stay in bounds
		let len = (((capacity + 7) / 8) * 8) as usize;
		let n = (len + CHUNK - 1) / CHUNK;
		self.slots = (0..n).map(|_| Slot::Zero).collect();
		self.rd = vec![ZERO_CHUNK.as_ptr(); n];
		self.wr = vec![std::ptr::null_mut(); n];
		self.len = len;
	}

	pub fn len(&self) -> usize {
		self.len
	}

	/// risc-box patch: take a shared image as this machine's memory. Every
	/// chunk the image holds becomes Shared (copied on first write); the
	/// rest stay Zero. The image's length must match `init`'s.
	pub fn adopt(&mut self, image: &RamImage) {
		assert_eq!(image.len, self.len, "RamImage size must match the machine's RAM");
		for (i, c) in image.chunks.iter().enumerate() {
			let slot = match c {
				Some(a) => Slot::Shared(a.clone()),
				None => Slot::Zero
			};
			self.set_slot(i, slot);
		}
	}

	/// risc-box patch: publish this machine's memory as an image others can
	/// fork from. Owned chunks are moved into shared ones (one copy each);
	/// this machine keeps reading them as Shared until it writes.
	pub fn share(&mut self) -> RamImage {
		let mut chunks = Vec::with_capacity(self.slots.len());
		for i in 0..self.slots.len() {
			let arc = match std::mem::replace(&mut self.slots[i], Slot::Zero) {
				Slot::Zero => None,
				Slot::Shared(a) => Some(a),
				Slot::Owned(b) => {
					let mut a = zero_arc();
					Arc::get_mut(&mut a).expect("fresh").copy_from_slice(&b[..]);
					Some(a)
				}
			};
			let slot = match &arc {
				Some(a) => Slot::Shared(a.clone()),
				None => Slot::Zero
			};
			self.set_slot(i, slot);
			chunks.push(arc);
		}
		RamImage { chunks, len: self.len }
	}

	/// Bytes this machine holds itself (owned chunks), and bytes it shares.
	pub fn footprint(&self) -> (usize, usize) {
		let mut owned = 0;
		let mut shared = 0;
		for s in &self.slots {
			match s {
				Slot::Owned(_) => owned += CHUNK,
				Slot::Shared(_) => shared += CHUNK,
				Slot::Zero => {}
			}
		}
		(owned, shared)
	}

	/// One PAGE of DRAM, or None when it has never been written (all zero).
	pub fn page(&self, page: usize) -> Option<&[u8]> {
		let addr = page * PAGE;
		let c = self.slots.get(addr >> CHUNK_SHIFT)?.bytes()?;
		let off = addr & (CHUNK - 1);
		Some(&c[off..off + PAGE])
	}

	/// Overwrite one PAGE (a restore into a live machine).
	pub fn write_page(&mut self, page: usize, data: &[u8]) {
		let addr = page * PAGE;
		let off = addr & (CHUNK - 1);
		let c = self.owned(addr >> CHUNK_SHIFT);
		c[off..off + data.len()].copy_from_slice(data);
	}

	/// The chunk holding `ci`, made writable: allocated if Zero, copied if
	/// Shared. The common case (already Owned) is one match arm.
	#[inline(always)]
	fn owned(&mut self, ci: usize) -> &mut Chunk {
		if self.wr[ci].is_null() {
			self.materialize(ci);
		}
		match &mut self.slots[ci] {
			Slot::Owned(b) => b,
			_ => unreachable!()
		}
	}

	/// The slow half of a write: allocate (Zero) or copy (Shared) into an
	/// owned chunk. Out of line so the write fast paths stay small.
	#[inline(never)]
	fn materialize(&mut self, ci: usize) {
		let mut fresh = zero_box();
		if let Slot::Shared(a) = &self.slots[ci] {
			fresh.copy_from_slice(&a[..]);
		}
		self.set_slot(ci, Slot::Owned(fresh));
	}

	#[inline(always)]
	fn chunk(&self, ci: usize) -> Option<&Chunk> {
		self.slots[ci].bytes()
	}

	/// Read `N` bytes at `a` (must not cross a chunk): one pointer load,
	/// one unaligned load.
	#[inline(always)]
	fn rd_at<const N: usize>(&self, a: usize) -> [u8; N] {
		let p = self.rd[a >> CHUNK_SHIFT];
		// SAFETY: p points at a live CHUNK-byte allocation (see `rd`) and
		// the caller checked (a & (CHUNK-1)) + N <= CHUNK.
		unsafe { std::ptr::read_unaligned(p.add(a & (CHUNK - 1)) as *const [u8; N]) }
	}

	/// Write `N` bytes at `a` (must not cross a chunk) into an owned chunk.
	#[inline(always)]
	fn wr_at<const N: usize>(&mut self, a: usize, v: [u8; N]) {
		let ci = a >> CHUNK_SHIFT;
		let mut p = self.wr[ci];
		if p.is_null() {
			self.materialize(ci);
			p = self.wr[ci];
		}
		// SAFETY: p points at this machine's own live CHUNK-byte allocation
		// (see `wr`) and the caller checked (a & (CHUNK-1)) + N <= CHUNK.
		unsafe { std::ptr::write_unaligned(p.add(a & (CHUNK - 1)) as *mut [u8; N], v) }
	}

	/// Reads a byte from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_byte(&self, address: u64) -> u8 {
		self.rd_at::<1>(address as usize)[0]
	}

	/// Reads two bytes from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_halfword(&self, address: u64) -> u16 {
		let a = address as usize;
		if (a & (CHUNK - 1)) + 2 <= CHUNK {
			return u16::from_le_bytes(self.rd_at::<2>(a));
		}
		self.read_bytes(address, 2) as u16
	}

	/// Reads four bytes from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_word(&self, address: u64) -> u32 {
		let a = address as usize;
		if (a & (CHUNK - 1)) + 4 <= CHUNK {
			return u32::from_le_bytes(self.rd_at::<4>(a));
		}
		self.read_bytes(address, 4) as u32
	}

	/// Reads eight bytes from memory.
	///
	/// # Arguments
	/// * `address`
	#[inline(always)]
	pub fn read_doubleword(&self, address: u64) -> u64 {
		let a = address as usize;
		if (a & (CHUNK - 1)) + 8 <= CHUNK {
			return u64::from_le_bytes(self.rd_at::<8>(a));
		}
		self.read_bytes(address, 8)
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
	/// into `out` — the framebuffer scanout path. Chunk by chunk; a Zero
	/// chunk is a memset.
	pub fn read_range(&self, address: u64, out: &mut [u8]) {
		let mut a = address as usize;
		let mut done = 0usize;
		while done < out.len() {
			let off = a & (CHUNK - 1);
			let n = (CHUNK - off).min(out.len() - done);
			match self.chunk(a >> CHUNK_SHIFT) {
				Some(c) => out[done..done + n].copy_from_slice(&c[off..off + n]),
				None => out[done..done + n].iter_mut().for_each(|b| *b = 0)
			}
			done += n;
			a += n;
		}
	}

	/// Writes a byte to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_byte(&mut self, address: u64, value: u8) {
		self.wr_at::<1>(address as usize, [value]);
	}

	/// Writes two bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_halfword(&mut self, address: u64, value: u16) {
		let a = address as usize;
		if (a & (CHUNK - 1)) + 2 <= CHUNK {
			return self.wr_at::<2>(a, value.to_le_bytes());
		}
		self.write_bytes(address, value as u64, 2);
	}

	/// Writes four bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_word(&mut self, address: u64, value: u32) {
		let a = address as usize;
		if (a & (CHUNK - 1)) + 4 <= CHUNK {
			return self.wr_at::<4>(a, value.to_le_bytes());
		}
		self.write_bytes(address, value as u64, 4);
	}

	/// Writes eight bytes to memory.
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	#[inline(always)]
	pub fn write_doubleword(&mut self, address: u64, value: u64) {
		let a = address as usize;
		if (a & (CHUNK - 1)) + 8 <= CHUNK {
			return self.wr_at::<8>(a, value.to_le_bytes());
		}
		self.write_bytes(address, value, 8);
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
		return (address as usize) < self.len
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn zero_until_written_and_straddles_work() {
		let mut m = Memory::new();
		m.init(3 * CHUNK as u64);
		assert_eq!(m.footprint(), (0, 0), "nothing allocated until a write");
		assert_eq!(m.read_doubleword(CHUNK as u64 - 4), 0);
		// a doubleword across the chunk boundary, written and read back
		m.write_doubleword(CHUNK as u64 - 4, 0x1122_3344_5566_7788);
		assert_eq!(m.read_doubleword(CHUNK as u64 - 4), 0x1122_3344_5566_7788);
		assert_eq!(m.read_word(CHUNK as u64 - 4), 0x5566_7788);
		assert_eq!(m.read_word(CHUNK as u64), 0x1122_3344);
		assert_eq!(m.footprint(), (2 * CHUNK, 0));
		let mut out = vec![0u8; 16];
		m.read_range(CHUNK as u64 - 8, &mut out);
		assert_eq!(&out[4..12], &0x1122_3344_5566_7788u64.to_le_bytes());
		assert!(m.page(0).is_some() && m.page(2 * CHUNK / PAGE).is_none());
	}

	#[test]
	fn fork_shares_until_written() {
		let mut root = Memory::new();
		root.init(2 * CHUNK as u64);
		root.write_word(100, 0xdead_beef);
		root.write_word(CHUNK as u64 + 8, 0xcafe_f00d);
		let image = root.share();
		assert_eq!(root.footprint(), (0, 2 * CHUNK), "publishing turns owned into shared");
		let mut a = Memory::new();
		a.init(2 * CHUNK as u64);
		a.adopt(&image);
		let mut b = Memory::new();
		b.init(2 * CHUNK as u64);
		b.adopt(&image);
		assert_eq!(a.read_word(100), 0xdead_beef);
		assert_eq!(b.read_word(CHUNK as u64 + 8), 0xcafe_f00d);
		assert_eq!(a.footprint(), (0, 2 * CHUNK));
		// a write in one fork copies that chunk and is invisible to the others
		a.write_word(100, 1);
		assert_eq!(a.read_word(100), 1);
		assert_eq!(b.read_word(100), 0xdead_beef);
		assert_eq!(root.read_word(100), 0xdead_beef);
		assert_eq!(a.footprint(), (CHUNK, CHUNK));
		assert_eq!(b.footprint(), (0, 2 * CHUNK));
		// the image itself is untouched and can seed more
		let mut c = Memory::new();
		c.init(2 * CHUNK as u64);
		c.adopt(&image);
		assert_eq!(c.read_word(100), 0xdead_beef);
		assert_eq!(image.footprint(), 2 * CHUNK);
	}

	#[test]
	fn pages_round_trip_through_an_image() {
		let mut img = RamImage::new(CHUNK);
		let data: Vec<u8> = (0..PAGE).map(|i| (i * 31 % 251) as u8).collect();
		img.write_page(3, &data);
		assert!(img.page(2).is_some(), "the chunk exists once any page in it does");
		assert_eq!(img.page(3).unwrap(), &data[..]);
		let mut m = Memory::new();
		m.init(CHUNK as u64);
		m.adopt(&img);
		assert_eq!(m.read_byte(3 * PAGE as u64 + 7), data[7]);
		m.write_page(3, &vec![9u8; PAGE]);
		assert_eq!(m.read_byte(3 * PAGE as u64 + 7), 9);
		assert_eq!(img.page(3).unwrap()[7], data[7], "the image is copy-on-write");
	}
}
