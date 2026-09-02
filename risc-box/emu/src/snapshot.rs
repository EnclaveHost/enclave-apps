// risc-box patch: whole-machine snapshot and restore.
//
// A booted desktop is ~100 s of emulated time away from a cold start; a
// snapshot makes that a fetch and an inflate instead. The format is a flat
// sequence of tagged sections, each device serializing its own state through
// the `Ser`/`De` helpers here so a layout change is confined to the device
// that made it. Guest RAM and the disk delta are the only large things and
// get the sparse, chunked codec at the bottom of this file.
//
// What a snapshot contains: the architectural CPU state, every device's
// registers and rings, the DTB as the guest saw it, guest DRAM (zero pages
// elided, the rest deflated), and the disk BLOCKS the guest changed since
// the base image was loaded. It does not contain the base disk image: the
// app fetches that exactly as for a cold boot and the delta is applied on
// top, which keeps a snapshot a fraction of the image's size. The caller
// binds the two with an opaque identity string (the app hashes the objects
// it fetched); a restore against a different base refuses rather than
// mounting a filesystem whose blocks are half from another image.
//
// What it deliberately leaves out: derived caches (TLB, predecoded blocks,
// AOT slots), which are rebuilt lazily; host-side queues (terminal bytes,
// ethernet frames in flight), which are dropped; and anything that is a
// host socket, which cannot survive a process boundary anyway.
//
// FORMAT must be bumped whenever any device's snapshot()/restore() pair
// changes what it writes. Every reader here checks lengths and runs the
// section to its end, so a drift that is not bumped fails loudly at restore
// rather than resuming a machine with one device's registers shifted.

use miniz_oxide::deflate::compress_to_vec;
use miniz_oxide::inflate::decompress_slice_iter_to_slice;

pub const FORMAT: u32 = 1;
const MAGIC: &[u8; 8] = b"RBXSNAP\0";

/// Page granularity of the sparse RAM codec and the disk delta.
pub const PAGE: usize = 4096;

/// Bytes of raw data per deflate record. Large enough that deflate has
/// context to work with, small enough that the inflate buffer on the
/// restore side is a rounding error next to the RAM being restored.
const CHUNK: usize = 8 * 1024 * 1024;

// ---- byte codec -------------------------------------------------------------

/// Little-endian writer. Every scalar has one width; there is no varint,
/// because the payloads that matter are the bulk ones below.
pub struct Ser {
	pub buf: Vec<u8>,
}

impl Ser {
	pub fn new() -> Self {
		Ser { buf: Vec::new() }
	}

	pub fn with_capacity(n: usize) -> Self {
		Ser { buf: Vec::with_capacity(n) }
	}

	pub fn u8(&mut self, v: u8) {
		self.buf.push(v);
	}

	pub fn bool(&mut self, v: bool) {
		self.buf.push(v as u8);
	}

	pub fn u16(&mut self, v: u16) {
		self.buf.extend_from_slice(&v.to_le_bytes());
	}

	pub fn u32(&mut self, v: u32) {
		self.buf.extend_from_slice(&v.to_le_bytes());
	}

	pub fn i32(&mut self, v: i32) {
		self.buf.extend_from_slice(&v.to_le_bytes());
	}

	pub fn u64(&mut self, v: u64) {
		self.buf.extend_from_slice(&v.to_le_bytes());
	}

	pub fn i64(&mut self, v: i64) {
		self.buf.extend_from_slice(&v.to_le_bytes());
	}

	pub fn f64(&mut self, v: f64) {
		self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
	}

	/// Length-prefixed bytes.
	pub fn bytes(&mut self, v: &[u8]) {
		self.u64(v.len() as u64);
		self.buf.extend_from_slice(v);
	}

	pub fn str(&mut self, v: &str) {
		self.bytes(v.as_bytes());
	}

	/// Bytes with no prefix: the caller's layout says how long they are.
	pub fn raw(&mut self, v: &[u8]) {
		self.buf.extend_from_slice(v);
	}

	/// Open a section: writes the tag and a placeholder length, returns the
	/// position `end_section` patches.
	pub fn begin_section(&mut self, tag: &[u8; 4]) -> usize {
		self.buf.extend_from_slice(tag);
		let at = self.buf.len();
		self.u64(0);
		at
	}

	pub fn end_section(&mut self, at: usize) {
		let len = (self.buf.len() - at - 8) as u64;
		self.buf[at..at + 8].copy_from_slice(&len.to_le_bytes());
	}

	pub fn section(&mut self, tag: &[u8; 4], payload: &[u8]) {
		self.buf.extend_from_slice(tag);
		self.u64(payload.len() as u64);
		self.buf.extend_from_slice(payload);
	}

	pub fn header(&mut self) {
		self.buf.extend_from_slice(MAGIC);
		self.u32(FORMAT);
	}
}

/// The reader. Every accessor is bounds-checked and returns an error
/// string rather than panicking: a snapshot is fetched from a bucket the
/// operator controls, and a short or hostile object must fail the restore,
/// never the host process.
pub struct De<'a> {
	data: &'a [u8],
	pos: usize,
}

impl<'a> De<'a> {
	pub fn new(data: &'a [u8]) -> Self {
		De { data, pos: 0 }
	}

	pub fn remaining(&self) -> usize {
		self.data.len() - self.pos
	}

	pub fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
		if n > self.remaining() {
			return Err(format!(
				"snapshot: truncated (wanted {} bytes at offset {}, {} left)",
				n, self.pos, self.remaining()
			));
		}
		let s = &self.data[self.pos..self.pos + n];
		self.pos += n;
		Ok(s)
	}

	pub fn u8(&mut self) -> Result<u8, String> {
		Ok(self.take(1)?[0])
	}

	pub fn bool(&mut self) -> Result<bool, String> {
		Ok(self.u8()? != 0)
	}

	pub fn u16(&mut self) -> Result<u16, String> {
		let b = self.take(2)?;
		Ok(u16::from_le_bytes([b[0], b[1]]))
	}

	pub fn u32(&mut self) -> Result<u32, String> {
		let b = self.take(4)?;
		Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
	}

	pub fn i32(&mut self) -> Result<i32, String> {
		Ok(self.u32()? as i32)
	}

	pub fn u64(&mut self) -> Result<u64, String> {
		let b = self.take(8)?;
		let mut a = [0u8; 8];
		a.copy_from_slice(b);
		Ok(u64::from_le_bytes(a))
	}

	pub fn i64(&mut self) -> Result<i64, String> {
		Ok(self.u64()? as i64)
	}

	pub fn f64(&mut self) -> Result<f64, String> {
		Ok(f64::from_bits(self.u64()?))
	}

	pub fn bytes(&mut self) -> Result<&'a [u8], String> {
		let n = self.u64()?;
		if n > self.remaining() as u64 {
			return Err(format!("snapshot: byte field of {} bytes overruns the section", n));
		}
		self.take(n as usize)
	}

	pub fn str(&mut self) -> Result<&'a str, String> {
		std::str::from_utf8(self.bytes()?).map_err(|_| "snapshot: string field is not UTF-8".to_string())
	}

	/// A section must be consumed exactly: leftover bytes mean the writer
	/// and reader disagree about the layout, which is the one thing a
	/// restore must never paper over.
	pub fn finish(&self, what: &str) -> Result<(), String> {
		match self.remaining() {
			0 => Ok(()),
			n => Err(format!("snapshot: {} section has {} unread bytes (format drift?)", what, n)),
		}
	}
}

/// Validate the container and split it into (tag, payload) pairs.
pub fn sections(data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>, String> {
	let mut r = De::new(data);
	let magic = r.take(MAGIC.len()).map_err(|_| "snapshot: not a RISC Box snapshot (too short)".to_string())?;
	if magic != MAGIC {
		return Err("snapshot: not a RISC Box snapshot (bad magic)".into());
	}
	let format = r.u32()?;
	if format != FORMAT {
		return Err(format!(
			"snapshot: format {} but this emulator writes format {} (take a fresh snapshot)",
			format, FORMAT
		));
	}
	let mut out = Vec::new();
	while r.remaining() > 0 {
		let t = r.take(4)?;
		let tag = [t[0], t[1], t[2], t[3]];
		let len = r.u64()?;
		if len > r.remaining() as u64 {
			return Err(format!(
				"snapshot: section {:?} claims {} bytes, {} remain (truncated download?)",
				String::from_utf8_lossy(&tag), len, r.remaining()
			));
		}
		out.push((tag, r.take(len as usize)?));
	}
	Ok(out)
}

// ---- chunked deflate --------------------------------------------------------

/// Accumulates raw bytes and writes them as `[u32 raw_len][bytes deflated]`
/// records once CHUNK has gathered (or at `finish`). The record count is
/// written FIRST by the caller, from `records()`, so the reader knows when
/// the stream ends without a sentinel.
pub struct ChunkWriter {
	level: u8,
	pending: Vec<u8>,
	records: Vec<u8>,
	count: u32,
	raw_total: u64,
}

impl ChunkWriter {
	pub fn new(level: u8) -> Self {
		ChunkWriter {
			level: level.min(10),
			pending: Vec::with_capacity(CHUNK),
			records: Vec::new(),
			count: 0,
			raw_total: 0,
		}
	}

	pub fn push(&mut self, data: &[u8]) {
		let mut rest = data;
		while !rest.is_empty() {
			let room = CHUNK - self.pending.len();
			let n = room.min(rest.len());
			self.pending.extend_from_slice(&rest[..n]);
			rest = &rest[n..];
			if self.pending.len() == CHUNK {
				self.flush();
			}
		}
	}

	fn flush(&mut self) {
		if self.pending.is_empty() {
			return;
		}
		let comp = compress_to_vec(&self.pending, self.level);
		self.records.extend_from_slice(&(self.pending.len() as u32).to_le_bytes());
		self.records.extend_from_slice(&(comp.len() as u64).to_le_bytes());
		self.records.extend_from_slice(&comp);
		self.raw_total += self.pending.len() as u64;
		self.count += 1;
		self.pending.clear();
	}

	/// Write the stream into `out`: record count, raw total, then the records.
	pub fn finish(mut self, out: &mut Ser) {
		self.flush();
		out.u32(self.count);
		out.u64(self.raw_total);
		out.raw(&self.records);
	}
}

/// The reader side: hands back the raw stream in whatever slice sizes the
/// caller asks for, inflating one record at a time into a reused buffer.
pub struct ChunkReader<'a> {
	de: De<'a>,
	left: u32,
	raw_total: u64,
	buf: Vec<u8>,
	off: usize,
}

impl<'a> ChunkReader<'a> {
	pub fn new(mut de: De<'a>) -> Result<Self, String> {
		let left = de.u32()?;
		let raw_total = de.u64()?;
		Ok(ChunkReader { de, left, raw_total, buf: Vec::new(), off: 0 })
	}

	pub fn raw_total(&self) -> u64 {
		self.raw_total
	}

	fn next_record(&mut self) -> Result<(), String> {
		if self.left == 0 {
			return Err("snapshot: compressed stream ended early".into());
		}
		self.left -= 1;
		let raw_len = self.de.u32()? as usize;
		if raw_len > CHUNK {
			return Err(format!("snapshot: record claims {} raw bytes (limit {})", raw_len, CHUNK));
		}
		let comp = self.de.bytes()?;
		self.buf.clear();
		self.buf.resize(raw_len, 0);
		let n = decompress_slice_iter_to_slice(&mut self.buf, std::iter::once(comp), false, true)
			.map_err(|e| format!("snapshot: inflate failed ({:?})", e))?;
		if n != raw_len {
			return Err(format!("snapshot: record inflated to {} bytes, expected {}", n, raw_len));
		}
		self.off = 0;
		Ok(())
	}

	pub fn read_exact(&mut self, out: &mut [u8]) -> Result<(), String> {
		let mut done = 0usize;
		while done < out.len() {
			if self.off == self.buf.len() {
				self.next_record()?;
			}
			let n = (self.buf.len() - self.off).min(out.len() - done);
			out[done..done + n].copy_from_slice(&self.buf[self.off..self.off + n]);
			self.off += n;
			done += n;
		}
		Ok(())
	}

	/// Everything must have been consumed: leftover records mean the
	/// caller's idea of the payload disagrees with what was written.
	pub fn finish(self, what: &str) -> Result<(), String> {
		if self.left != 0 || self.off != self.buf.len() {
			return Err(format!("snapshot: {} stream has unread data (format drift?)", what));
		}
		self.de.finish(what)
	}
}

/// What the MMU's snapshot writer reports back up to the Emulator.
pub struct SnapshotStats {
	pub ram: RamStats,
	pub delta_blocks: usize,
}

/// Accumulated by the section readers so the Emulator can report them.
#[derive(Default)]
pub struct RestoreStats {
	pub ram_pages_kept: u64,
	pub ram_pages_total: u64,
	pub delta_blocks: usize,
}

// ---- sparse RAM -------------------------------------------------------------

pub struct RamStats {
	pub bytes: u64,
	pub pages_kept: u64,
	pub pages_total: u64,
}

/// A page of guest DRAM that has never been touched (or has been freed and
/// zeroed) is all zero, and on a booted machine that is most of them: the
/// bitmap names the pages that are not, and only those go through deflate.
pub fn encode_ram(ram: &[u8], level: u8, out: &mut Ser) -> RamStats {
	let pages = (ram.len() + PAGE - 1) / PAGE;
	let mut bitmap = vec![0u8; (pages + 7) / 8];
	let mut kept = 0u64;
	let mut w = ChunkWriter::new(level);
	for (i, page) in ram.chunks(PAGE).enumerate() {
		if page.iter().all(|&b| b == 0) {
			continue;
		}
		bitmap[i >> 3] |= 1 << (i & 7);
		kept += 1;
		w.push(page);
	}
	out.u64(ram.len() as u64);
	out.u32(PAGE as u32);
	out.bytes(&bitmap);
	w.finish(out);
	RamStats { bytes: ram.len() as u64, pages_kept: kept, pages_total: pages as u64 }
}

/// The inverse: `ram` must already be the right size and is zeroed by the
/// caller (a fresh allocation is); only the pages the bitmap names are
/// written.
pub fn decode_ram(payload: &[u8], ram: &mut [u8]) -> Result<RamStats, String> {
	let mut de = De::new(payload);
	let len = de.u64()?;
	if len != ram.len() as u64 {
		return Err(format!(
			"snapshot: guest RAM is {} bytes but the machine was allocated {}",
			len, ram.len()
		));
	}
	let page = de.u32()? as usize;
	if page != PAGE {
		return Err(format!("snapshot: RAM page size {} unsupported", page));
	}
	let pages = (ram.len() + PAGE - 1) / PAGE;
	let bitmap = de.bytes()?;
	if bitmap.len() != (pages + 7) / 8 {
		return Err("snapshot: RAM bitmap length does not match RAM size".into());
	}
	let mut r = ChunkReader::new(de)?;
	let mut kept = 0u64;
	for (i, page) in ram.chunks_mut(PAGE).enumerate() {
		if bitmap[i >> 3] & (1 << (i & 7)) == 0 {
			continue;
		}
		r.read_exact(page)?;
		kept += 1;
	}
	r.finish("RAM")?;
	Ok(RamStats { bytes: len, pages_kept: kept, pages_total: pages as u64 })
}

// ---- disk delta -------------------------------------------------------------

/// Blocks the guest wrote since the base image was loaded, by index, each
/// PAGE bytes. `read` fills one block; the caller decides what a block is
/// (the virtio-blk device tracks them at PAGE granularity).
pub fn encode_delta(blocks: &[u32], level: u8, mut read: impl FnMut(u32, &mut [u8]), out: &mut Ser) {
	out.u32(PAGE as u32);
	out.u32(blocks.len() as u32);
	for &b in blocks {
		out.u32(b);
	}
	let mut w = ChunkWriter::new(level);
	let mut buf = vec![0u8; PAGE];
	for &b in blocks {
		read(b, &mut buf);
		w.push(&buf);
	}
	w.finish(out);
}

/// Apply a delta: `write` is called once per block, in the order written.
pub fn decode_delta(payload: &[u8], mut write: impl FnMut(u32, &[u8]) -> Result<(), String>) -> Result<u32, String> {
	let mut de = De::new(payload);
	let page = de.u32()? as usize;
	if page != PAGE {
		return Err(format!("snapshot: delta block size {} unsupported", page));
	}
	let n = de.u32()?;
	let mut blocks = Vec::with_capacity(n as usize);
	for _ in 0..n {
		blocks.push(de.u32()?);
	}
	let mut r = ChunkReader::new(de)?;
	let mut buf = vec![0u8; PAGE];
	for &b in &blocks {
		r.read_exact(&mut buf)?;
		write(b, &buf)?;
	}
	r.finish("disk delta")?;
	Ok(n)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn scalars_round_trip() {
		let mut s = Ser::new();
		s.u8(7);
		s.bool(true);
		s.u16(0xbeef);
		s.u32(0xdead_beef);
		s.i32(-5);
		s.u64(u64::MAX - 1);
		s.i64(-9);
		s.f64(3.5);
		s.bytes(b"hello");
		s.str("wörld");
		let mut d = De::new(&s.buf);
		assert_eq!(d.u8().unwrap(), 7);
		assert!(d.bool().unwrap());
		assert_eq!(d.u16().unwrap(), 0xbeef);
		assert_eq!(d.u32().unwrap(), 0xdead_beef);
		assert_eq!(d.i32().unwrap(), -5);
		assert_eq!(d.u64().unwrap(), u64::MAX - 1);
		assert_eq!(d.i64().unwrap(), -9);
		assert_eq!(d.f64().unwrap(), 3.5);
		assert_eq!(d.bytes().unwrap(), b"hello");
		assert_eq!(d.str().unwrap(), "wörld");
		d.finish("test").unwrap();
		assert!(d.u8().is_err(), "reading past the end must error, not panic");
	}

	#[test]
	fn sections_split_and_validate() {
		let mut s = Ser::new();
		s.header();
		s.section(b"AAAA", b"one");
		let at = s.begin_section(b"BBBB");
		s.u32(42);
		s.end_section(at);
		let v = sections(&s.buf).unwrap();
		assert_eq!(v.len(), 2);
		assert_eq!(&v[0].0, b"AAAA");
		assert_eq!(v[0].1, b"one");
		assert_eq!(&v[1].0, b"BBBB");
		assert_eq!(v[1].1, &42u32.to_le_bytes());
		// truncated container
		assert!(sections(&s.buf[..s.buf.len() - 2]).is_err());
		// wrong magic
		assert!(sections(b"nope").is_err());
		// wrong format
		let mut bad = s.buf.clone();
		bad[8] = 99;
		assert!(sections(&bad).unwrap_err().contains("format"));
	}

	fn diskish(n: usize, seed: u64) -> Vec<u8> {
		let mut v = vec![0u8; n];
		let mut x = seed;
		// sparse: touch one in ~5 pages, a few bytes each, plus one dense page
		for p in (0..n / PAGE).step_by(5) {
			for k in 0..16 {
				x ^= x << 13;
				x ^= x >> 7;
				x ^= x << 17;
				v[p * PAGE + (x as usize % PAGE)] = (x >> 8) as u8 | 1;
				let _ = k;
			}
		}
		if n >= 2 * PAGE {
			for b in &mut v[PAGE..2 * PAGE] {
				x ^= x << 13;
				x ^= x >> 7;
				x ^= x << 17;
				*b = x as u8;
			}
		}
		v
	}

	#[test]
	fn ram_round_trip_is_sparse() {
		for n in [3 * PAGE, 7 * PAGE + 100, 3 * CHUNK + 5 * PAGE] {
			let ram = diskish(n, 0x1234_5678_9abc_def1);
			let mut s = Ser::new();
			let st = encode_ram(&ram, 1, &mut s);
			assert!(st.pages_kept < st.pages_total, "zero pages must be elided at n={}", n);
			if n >= 7 * PAGE {
				assert!(s.buf.len() < ram.len() / 2, "sparse RAM must compress at n={}", n);
			}
			let mut back = vec![0u8; n];
			let st2 = decode_ram(&s.buf, &mut back).unwrap();
			assert_eq!(st2.pages_kept, st.pages_kept);
			assert_eq!(back, ram, "round trip at n={}", n);
			// a machine allocated a different size must refuse
			let mut wrong = vec![0u8; n + PAGE];
			assert!(decode_ram(&s.buf, &mut wrong).is_err());
		}
	}

	#[test]
	fn chunk_reader_spans_records() {
		let data: Vec<u8> = (0..(2 * CHUNK + 777)).map(|i| (i * 7 % 251) as u8).collect();
		let mut w = ChunkWriter::new(1);
		w.push(&data[..1000]);
		w.push(&data[1000..]);
		let mut s = Ser::new();
		w.finish(&mut s);
		let mut r = ChunkReader::new(De::new(&s.buf)).unwrap();
		assert_eq!(r.raw_total(), data.len() as u64);
		let mut out = vec![0u8; data.len()];
		// odd read sizes straddle record boundaries on purpose
		let mut off = 0;
		let mut k = 0usize;
		while off < out.len() {
			let n = (12345 + k * 1013).min(out.len() - off);
			r.read_exact(&mut out[off..off + n]).unwrap();
			off += n;
			k += 1;
		}
		assert_eq!(out, data);
		r.finish("test").unwrap();
	}

	#[test]
	fn delta_round_trip() {
		let disk: Vec<u8> = diskish(64 * PAGE, 99);
		let dirty = [3u32, 17, 40, 63];
		let mut s = Ser::new();
		encode_delta(&dirty, 1, |b, out| out.copy_from_slice(&disk[b as usize * PAGE..(b as usize + 1) * PAGE]), &mut s);
		let mut got: Vec<(u32, Vec<u8>)> = Vec::new();
		let n = decode_delta(&s.buf, |b, data| {
			got.push((b, data.to_vec()));
			Ok(())
		})
		.unwrap();
		assert_eq!(n, 4);
		for (i, (b, data)) in got.iter().enumerate() {
			assert_eq!(*b, dirty[i]);
			assert_eq!(data, &disk[*b as usize * PAGE..(*b as usize + 1) * PAGE]);
		}
	}
}
