//! risc-box patch: the disk's shared, read-only base image, held compressed.
//!
//! The image used to be one expanded `Vec<u8>`: 3 GiB for the Alpine desktop, inside a shared wasm memory that the
//! platform's partition runtime lets grow to 4 GiB (no reservation beyond wasmtime's default). That left the guest's
//! RAM, the JIT and every buffer ~1 GiB between them, and a busy Firefox crossed it: the allocator failed, the app
//! aborted, and the node retired the machine. Held as 64 KiB chunks — all-zero chunks as nothing, the rest LZ4 —
//! the same image is about a third of that, and a read costs one chunk decompression on a cache miss.
//!
//! Only the base is here. Writes still land in VirtioBlockDisk's 4 KiB overlay, and a snapshot still carries only
//! that overlay, so the snapshot format and every identity check are unchanged.

use std::sync::Mutex;

/// Chunk size: a multiple of the overlay's 4 KiB block, so a block never straddles two chunks.
pub const CHUNK: usize = 64 * 1024;

/// Decompressed chunks kept, direct-mapped by chunk index (8 MiB).
const CACHE_SLOTS: usize = 128;

static ZEROS: [u8; CHUNK] = [0; CHUNK];

enum Stored {
	Zero,
	Raw(Box<[u8]>),
	Lz4(Box<[u8]>),
}

struct Cache {
	tags: Vec<usize>,
	data: Vec<Box<[u8]>>,
}

pub struct DiskImage {
	len: usize,
	chunks: Vec<Stored>,
	stored_bytes: usize,
	cache: Mutex<Cache>,
}

impl DiskImage {
	pub fn empty() -> DiskImage {
		Builder::new().finish()
	}

	pub fn from_vec(v: Vec<u8>) -> DiskImage {
		let mut b = Builder::new();
		b.push(&v);
		b.finish()
	}

	/// Bytes of image, as the guest sees it.
	pub fn len(&self) -> usize {
		self.len
	}

	/// Bytes actually held for the chunks (compressed and raw; zero chunks hold none).
	pub fn stored_bytes(&self) -> usize {
		self.stored_bytes
	}

	fn chunk_len(&self, idx: usize) -> usize {
		(self.len - idx * CHUNK).min(CHUNK)
	}

	/// Run `f` over chunk `idx`'s bytes (decompressing it into the cache when needed).
	fn with_chunk<R>(&self, idx: usize, f: impl FnOnce(&[u8]) -> R) -> R {
		let n = self.chunk_len(idx);
		match &self.chunks[idx] {
			Stored::Zero => f(&ZEROS[..n]),
			Stored::Raw(b) => f(&b[..n]),
			Stored::Lz4(c) => {
				let mut g = self.cache.lock().unwrap_or_else(|e| e.into_inner());
				let slot = idx % CACHE_SLOTS;
				if g.tags[slot] != idx {
					let out = &mut g.data[slot][..n];
					let got = lz4_flex::block::decompress_into(c, out).expect("disk image: corrupt chunk");
					assert_eq!(got, n, "disk image: chunk {idx} inflated to {got} bytes, not {n}");
					g.tags[slot] = idx;
				}
				f(&g.data[slot][..n])
			}
		}
	}

	/// Fill `out` from `offset`; bytes past the end of the image read as zero.
	pub fn read(&self, offset: usize, out: &mut [u8]) {
		let mut done = 0;
		while done < out.len() {
			let at = offset + done;
			if at >= self.len {
				out[done..].fill(0);
				return;
			}
			let idx = at / CHUNK;
			let off = at % CHUNK;
			let take = (CHUNK - off).min(out.len() - done).min(self.len - at);
			self.with_chunk(idx, |c| out[done..done + take].copy_from_slice(&c[off..off + take]));
			done += take;
		}
	}

	pub fn read_u8(&self, offset: usize) -> u8 {
		if offset >= self.len {
			return 0;
		}
		self.with_chunk(offset / CHUNK, |c| c[offset % CHUNK])
	}

	pub fn read_u64(&self, offset: usize) -> u64 {
		let mut b = [0u8; 8];
		self.read(offset, &mut b);
		u64::from_le_bytes(b)
	}

	/// The whole image expanded (for /save, which uploads plain bytes).
	pub fn to_vec(&self) -> Vec<u8> {
		let mut v = vec![0u8; self.len];
		for idx in 0..self.chunks.len() {
			let at = idx * CHUNK;
			let n = self.chunk_len(idx);
			if !matches!(self.chunks[idx], Stored::Zero) {
				self.with_chunk(idx, |c| v[at..at + n].copy_from_slice(c));
			}
		}
		v
	}
}

/// Packs an image chunk by chunk as its bytes arrive, so the expanded image never exists in memory at once.
pub struct Builder {
	len: usize,
	chunks: Vec<Stored>,
	stored: usize,
	buf: Vec<u8>,
}

impl Builder {
	pub fn new() -> Builder {
		Builder { len: 0, chunks: Vec::new(), stored: 0, buf: Vec::with_capacity(CHUNK) }
	}

	pub fn push(&mut self, mut data: &[u8]) {
		while !data.is_empty() {
			let take = (CHUNK - self.buf.len()).min(data.len());
			self.buf.extend_from_slice(&data[..take]);
			data = &data[take..];
			if self.buf.len() == CHUNK {
				self.flush();
			}
		}
	}

	fn flush(&mut self) {
		let b = &self.buf;
		let zero = b.chunks(8).all(|w| w.iter().all(|&x| x == 0));
		let s = if zero {
			Stored::Zero
		} else {
			let c = lz4_flex::block::compress(b);
			// keep it raw when LZ4 cannot save an eighth (already-compressed data): reads skip the decompression
			if c.len() < b.len() - b.len() / 8 {
				self.stored += c.len();
				Stored::Lz4(c.into_boxed_slice())
			} else {
				self.stored += b.len();
				Stored::Raw(b.clone().into_boxed_slice())
			}
		};
		self.len += b.len();
		self.chunks.push(s);
		self.buf.clear();
	}

	/// Bytes pushed so far.
	pub fn len(&self) -> usize {
		self.len + self.buf.len()
	}

	pub fn finish(mut self) -> DiskImage {
		if !self.buf.is_empty() {
			self.flush();
		}
		self.chunks.shrink_to_fit();
		DiskImage {
			len: self.len,
			chunks: self.chunks,
			stored_bytes: self.stored,
			cache: Mutex::new(Cache {
				tags: vec![usize::MAX; CACHE_SLOTS],
				data: (0..CACHE_SLOTS).map(|_| vec![0u8; CHUNK].into_boxed_slice()).collect(),
			}),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::convert::TryInto;

	fn sample(len: usize) -> Vec<u8> {
		// zero runs, compressible text, incompressible noise, and a partial last chunk
		let mut v = vec![0u8; len];
		let mut x = 0x2545F4914F6CDD1Du64;
		for (i, b) in v.iter_mut().enumerate() {
			*b = match (i / CHUNK) % 4 {
				0 => 0,
				1 => b"risc-box disk image "[i % 20],
				2 => {
					x ^= x << 13;
					x ^= x >> 7;
					x ^= x << 17;
					x as u8
				}
				_ => if i % 3 == 0 { (i >> 9) as u8 } else { 0 },
			};
		}
		v
	}

	#[test]
	fn reads_match_the_expanded_image() {
		let len = 9 * CHUNK + 12345;
		let v = sample(len);
		let d = DiskImage::from_vec(v.clone());
		assert_eq!(d.len(), len);
		assert_eq!(d.to_vec(), v);
		assert!(d.stored_bytes() < len, "zero chunks hold nothing: {} of {}", d.stored_bytes(), len);
		let mut x = 7u64;
		for _ in 0..20000 {
			x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			let off = (x >> 20) as usize % (len + 100);
			let n = ((x >> 8) as usize % 9000) + 1;
			let mut got = vec![0xAAu8; n];
			d.read(off, &mut got);
			for (k, g) in got.iter().enumerate() {
				let want = v.get(off + k).copied().unwrap_or(0);
				assert_eq!(*g, want, "offset {}", off + k);
			}
			assert_eq!(d.read_u8(off), v.get(off).copied().unwrap_or(0));
		}
		for off in (0..len - 8).step_by(4093) {
			assert_eq!(d.read_u64(off), u64::from_le_bytes(v[off..off + 8].try_into().unwrap()));
		}
	}

	#[test]
	fn streaming_builder_equals_one_shot() {
		let v = sample(5 * CHUNK + 777);
		let mut b = Builder::new();
		for piece in v.chunks(10007) {
			b.push(piece);
		}
		assert_eq!(b.len(), v.len());
		let d = b.finish();
		assert_eq!(d.to_vec(), v);
		assert_eq!(DiskImage::empty().len(), 0);
	}
}
