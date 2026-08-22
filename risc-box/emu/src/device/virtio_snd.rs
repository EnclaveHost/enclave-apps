// risc-box patch: Virtio Sound device — the machine's sound card. Mapped at
// 0x10004000, IRQ 4 on the PLIC.
//
// Playback only: one PCM output stream, one jack, one channel map. That is
// the whole of what a remote desktop needs, and every part of the spec this
// leaves out (capture, multiple streams, jack remapping) is a request the
// driver only makes if the config space advertises it.
//
// Like virtio_input.rs this is a MODERN (virtio 1.0) mmio device — the Linux
// `virtio_snd` driver is 1.0-only — so the register file, the FEATURES_OK
// handshake and the split-virtqueue address registers mirror that file
// exactly; only the queues and the payloads differ.
//
// PACING is the one design decision worth stating. A real card completes a
// playback buffer when its DAC has consumed it, and that is what keeps ALSA's
// clock honest: complete instantly and the guest plays a 400 KB file in no
// time, so every sound comes out at the wrong speed. So playback is metered
// against the CLINT's mtime — the same monotonic 10 MHz tick the guest reads,
// and wall-clock backed when the machine runs realtime. Credit accrues at the
// stream's own byte rate and a buffer is completed only when its bytes are
// paid for.
//
// What must NOT happen is stalling on the host. An earlier cut completed a
// buffer only once its bytes fit in the outbound ring, so a guest playing to
// nobody wedged: the ring filled, completions stopped, and ALSA gave up with
// EIO ("write error: I/O error" out of aplay). A speaker no one listens to
// still consumes audio, so a full ring drops its OLDEST bytes and playback
// keeps its timing.
//
// Based on VIRTIO v1.2: section 5.14 (Sound Device).

use std::collections::VecDeque;

use mmu::MemoryWrapper;

const BASE: u64 = 0x10004000;
const MAX_QUEUE_SIZE: u32 = 256;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// Queue indices (5.14.2)
const CONTROL_QUEUE: usize = 0;
const _EVENT_QUEUE: usize = 1;
const TX_QUEUE: usize = 2;
const _RX_QUEUE: usize = 3;
const NUM_QUEUES: usize = 4;

// Request codes (5.14.6)
const R_JACK_INFO: u32 = 1;
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const R_CHMAP_INFO: u32 = 0x0200;

// Status codes
const S_OK: u32 = 0x8000;
const S_NOT_SUPP: u32 = 0x8002;

// Stream direction
const D_OUTPUT: u8 = 0;

// PCM format / rate bits we advertise (5.14.6.6.1)
const FMT_S16: u32 = 5;
const RATE_11025: u32 = 2;
const RATE_22050: u32 = 4;
const RATE_44100: u32 = 6;
const RATE_48000: u32 = 7;

/// Bytes of played audio held for the host.
///
/// ~500 ms at 11 kHz stereo, which is what the guest actually plays.
///
/// Depth here is NOT latency, as long as the host drains promptly: a listener
/// that keeps up leaves this nearly empty and the audio is as live as the
/// pushes are. What depth buys is tolerance for the app's occasional slow
/// turn. Sized at 150 ms it could not absorb one, and measured 11 kB/s thrown
/// away out of 38 kB/s produced — a third of the sound, heard as chopping —
/// while a listener was attached the entire time. Overflow still drops the
/// OLDEST, so a truly absent listener costs staleness, never the guest.
const RING_CAP: usize = 22_050;

/// The CLINT's tick rate, and the DTB's advertised timebase.
const MTIME_HZ: u64 = 10_000_000;

struct Queue {
	num: u32,
	ready: bool,
	desc: u64,
	driver: u64,
	device: u64,
	avail_cursor: u16,
	used_index: u16,
}

impl Queue {
	fn new() -> Self {
		Queue { num: 0, ready: false, desc: 0, driver: 0, device: 0, avail_cursor: 0, used_index: 0 }
	}
	fn is_ready(&self) -> bool {
		self.ready && self.num != 0 && self.desc != 0 && self.driver != 0 && self.device != 0
	}
}

pub struct VirtioSnd {
	device_features_sel: u32,
	driver_features: u64,
	driver_features_sel: u32,
	queue_select: u32,
	interrupt_status: u32,
	status: u32,
	queues: [Queue; NUM_QUEUES],
	/// PCM bytes the guest has written and the host has not taken yet.
	ring: VecDeque<u8>,
	/// Stream parameters the driver set, so the host knows how to interpret
	/// the ring. Defaults describe the stream we advertise, not silence.
	rate_hz: u32,
	channels: u8,
	/// Whether the driver has started the stream. The host uses this to tell
	/// "playing silence" from "not playing", which is the difference between
	/// sending Opus silence and sending nothing at all.
	running: bool,
	/// Playback credit in bytes, accrued from mtime at the stream's byte
	/// rate, and the mtime it was last accrued at. `credit_frac` carries the
	/// sub-byte remainder between calls; see accrue() for why that is not a
	/// rounding nicety but the whole of the stream.
	credit: u64,
	credit_frac: u64,
	last_mtime: u64,
	/// Guest bytes dropped because the ring filled before the host took them.
	/// Non-zero means nobody was listening (or was too slow), never that the
	/// guest misbehaved — playback timing is unaffected either way.
	dropped: u64,
}

impl VirtioSnd {
	pub fn new() -> Self {
		VirtioSnd {
			device_features_sel: 0,
			driver_features: 0,
			driver_features_sel: 0,
			queue_select: 0,
			interrupt_status: 0,
			status: 0,
			queues: [Queue::new(), Queue::new(), Queue::new(), Queue::new()],
			ring: VecDeque::new(),
			rate_hz: 48_000,
			channels: 2,
			running: false,
			credit: 0,
			credit_frac: 0,
			last_mtime: 0,
			dropped: 0,
		}
	}

	/// Host → app: take up to `max` bytes of played audio. Interleaved signed
	/// 16-bit little-endian at `rate_hz` / `channels`.
	pub fn take_pcm(&mut self, max: usize) -> Vec<u8> {
		// ALWAYS hand back whole frames. The ring holds a byte stream and a
		// take used to cut it wherever the cap or the contents landed, so a
		// chunk could begin mid-frame — after which every consumer that
		// assumes frame alignment has left and right swapped for that chunk.
		// Heard as a grainy, phasey edge on the sound, and it bit the music
		// mixer (src/opl.rs) and the Opus encoder alike. Alignment belongs
		// here, at the source, not in each consumer.
		let frame = (self.channels.max(1) as usize) * 2;
		let n = max.min(self.ring.len()) / frame * frame;
		self.ring.drain(..n).collect()
	}

	/// Bytes waiting to be taken — the host's cue that it is behind.
	pub fn pending_bytes(&self) -> usize {
		self.ring.len()
	}

	/// (sample rate, channels, playing) as the driver last set them.
	pub fn format(&self) -> (u32, u8, bool) {
		(self.rate_hz, self.channels, self.running)
	}

	pub fn dropped_bytes(&self) -> u64 {
		self.dropped
	}

	pub fn is_interrupting(&mut self) -> bool {
		let pending = (self.interrupt_status & 0x1) != 0;
		pending && self.driver_ready()
	}

	pub fn tick(&mut self, mtime: u64, memory: &mut MemoryWrapper) {
		if !self.driver_ready() {
			return;
		}
		self.accrue(mtime);
		self.drain_control(memory);
		self.drain_tx(memory);
	}

	/// Pay out playback credit for the time that has passed. Only while the
	/// stream runs: a stopped stream must not bank credit it would spend as a
	/// burst the moment it starts.
	fn accrue(&mut self, mtime: u64) {
		let last = self.last_mtime;
		self.last_mtime = mtime;
		if !self.running || last == 0 || mtime <= last {
			return;
		}
		let bytes_per_sec = self.rate_hz as u64 * self.channels as u64 * 2;
		// PAY THE REMAINDER, or pay almost nothing. tick() runs every 32
		// retired instructions — around three million times a second — so
		// mtime advances only about three of its 10 MHz ticks between calls,
		// and `delta * 44100 / 10_000_000` truncates to ZERO nearly every
		// time. Discarding that remainder threw away most of the stream: the
		// card played at a fraction of real time, so ALSA's buffer sat
		// permanently full (avail 0, 620 ms of delay) and the game's mixer
		// dropped whatever no longer fit. Carrying the fraction makes the
		// rate exact no matter how finely the device is serviced.
		self.credit_frac += (mtime - last) * bytes_per_sec;
		let paid = self.credit_frac / MTIME_HZ;
		self.credit_frac -= paid * MTIME_HZ;
		self.credit += paid;
		// A pause (or a host that stopped servicing us) must not turn into a
		// burst of instant completions afterwards.
		let cap = bytes_per_sec / 4;
		if self.credit > cap {
			self.credit = cap;
		}
	}

	fn driver_ready(&self) -> bool {
		(self.status & 4) != 0 // DRIVER_OK
	}

	// ---- control queue ---------------------------------------------------

	fn drain_control(&mut self, memory: &mut MemoryWrapper) {
		for _ in 0..MAX_QUEUE_SIZE {
			let Some(head) = self.pop_avail(memory, CONTROL_QUEUE) else { break };
			let (req, writable) = self.walk_chain(memory, CONTROL_QUEUE, head);
			let written = self.handle_control(&req, &writable, memory);
			self.push_used(memory, CONTROL_QUEUE, head, written);
		}
	}

	/// Answer one control request; returns bytes written back to the driver.
	fn handle_control(&mut self, req: &[u8], writable: &[(u64, u32)],
	                  memory: &mut MemoryWrapper) -> u32 {
		let code = le32(req, 0);
		match code {
			R_JACK_INFO => {
				let mut out = status_bytes(S_OK);
				// one jack: hda_fn_nid 0, no features, a line-out that is
				// always connected (nothing here can be unplugged)
				out.extend_from_slice(&0u32.to_le_bytes()); // hda_fn_nid
				out.extend_from_slice(&0u32.to_le_bytes()); // features
				out.extend_from_slice(&0x10u32.to_le_bytes()); // hda_reg_defconf
				out.extend_from_slice(&0u32.to_le_bytes()); // hda_reg_caps
				out.push(1); // connected
				out.extend_from_slice(&[0u8; 7]);
				write_out(memory, writable, &out)
			}
			R_PCM_INFO => {
				let mut out = status_bytes(S_OK);
				out.extend_from_slice(&0u32.to_le_bytes()); // hda_fn_nid
				out.extend_from_slice(&0u32.to_le_bytes()); // features
				let formats: u64 = 1 << FMT_S16;
				let rates: u64 = (1 << RATE_11025) | (1 << RATE_22050)
					| (1 << RATE_44100) | (1 << RATE_48000);
				out.extend_from_slice(&formats.to_le_bytes());
				out.extend_from_slice(&rates.to_le_bytes());
				out.push(D_OUTPUT);
				out.push(1); // channels_min
				out.push(2); // channels_max
				out.extend_from_slice(&[0u8; 5]);
				write_out(memory, writable, &out)
			}
			R_CHMAP_INFO => {
				let mut out = status_bytes(S_OK);
				out.extend_from_slice(&0u32.to_le_bytes()); // hda_fn_nid
				out.push(D_OUTPUT);
				out.push(2); // channels
				// VIRTIO_SND_CHMAP_FL = 3, FR = 4; rest unused
				let mut pos = [0u8; 18];
				pos[0] = 3;
				pos[1] = 4;
				out.extend_from_slice(&pos);
				write_out(memory, writable, &out)
			}
			R_PCM_SET_PARAMS => {
				// struct virtio_snd_pcm_set_params, after the 8-byte pcm hdr:
				// buffer_bytes, period_bytes, features, channels, format, rate
				self.channels = req.get(20).copied().unwrap_or(2).max(1).min(2);
				let rate_code = req.get(22).copied().unwrap_or(RATE_48000 as u8) as u32;
				self.rate_hz = match rate_code {
					RATE_11025 => 11_025,
					RATE_22050 => 22_050,
					RATE_44100 => 44_100,
					_ => 48_000,
				};
				write_out(memory, writable, &status_bytes(S_OK))
			}
			R_PCM_PREPARE => {
				// Belt and braces: a stream being prepared owns nothing yet.
				self.flush_tx(memory);
				// PREPARED, not RUNNING: START is what begins playback. Also
				// the xrun recovery path — ALSA re-prepares after an underrun,
				// so every scrap of old state has to go with it (a stale
				// credit would be spent as an instant burst on the restart).
				self.running = false;
				self.credit = 0;
				self.credit_frac = 0;
				self.last_mtime = 0;
				self.ring.clear();
				write_out(memory, writable, &status_bytes(S_OK))
			}
			R_PCM_START => {
				self.running = true;
				write_out(memory, writable, &status_bytes(S_OK))
			}
			R_PCM_STOP => {
				self.running = false;
				self.credit = 0;
				self.credit_frac = 0;
				write_out(memory, writable, &status_bytes(S_OK))
			}
			R_PCM_RELEASE => {
				self.running = false;
				self.credit = 0;
				self.credit_frac = 0;
				self.last_mtime = 0;
				self.ring.clear();
				// The spec requires every pending I/O buffer to come back on
				// release, and the driver reclaims its side regardless. Keep
				// the ones we never paid credit for and the two sides disagree
				// about how far into the avail ring they are — after which the
				// NEXT open's control message goes unanswered and ALSA reports
				// "audio open error: Operation timed out". One session worked,
				// every session after it did not.
				self.flush_tx(memory);
				write_out(memory, writable, &status_bytes(S_OK))
			}
			// Anything else (jack remap, capture) is refused rather than
			// ignored: a driver that gets S_OK for a request the device did
			// not honour goes on to use a stream that does not exist.
			_ => write_out(memory, writable, &status_bytes(S_NOT_SUPP)),
		}
	}

	/// Hand every posted playback buffer back, completed. Used when a stream
	/// is released or re-prepared, so the device and driver agree on where the
	/// avail ring stands.
	fn flush_tx(&mut self, memory: &mut MemoryWrapper) {
		for _ in 0..MAX_QUEUE_SIZE {
			let Some(head) = self.pop_avail(memory, TX_QUEUE) else { break };
			let (_data, writable) = self.walk_chain(memory, TX_QUEUE, head);
			let mut st = status_bytes(S_OK);
			st.extend_from_slice(&0u32.to_le_bytes());
			let written = write_out(memory, &writable, &st);
			self.push_used(memory, TX_QUEUE, head, written);
		}
	}

	// ---- tx queue (playback) --------------------------------------------

	fn drain_tx(&mut self, memory: &mut MemoryWrapper) {
		for _ in 0..MAX_QUEUE_SIZE {
			// Buffers posted before START are the driver's prefill: they are
			// held, exactly as a card holds them, until playback runs.
			if !self.running {
				break;
			}
			let Some(head) = self.peek_avail(memory, TX_QUEUE) else { break };
			// PRICE the buffer before reading it. walk_chain copies the whole
			// payload a byte at a time through the MMU, and until the credit is
			// there the answer is "not yet" — so doing it first meant copying
			// a ~5.5 KB period buffer on EVERY tick and discarding it. That
			// alone took the emulator from ~130 MIPS to 46 with sound playing.
			let pcm_len = self.readable_len(memory, TX_QUEUE, head).saturating_sub(4);
			// Not yet played: leave it posted and come back when time has
			// passed. This is the whole of the rate control.
			if self.credit < pcm_len {
				break;
			}
			self.credit -= pcm_len;
			let _ = self.pop_avail(memory, TX_QUEUE);
			let (data, writable) = self.walk_chain(memory, TX_QUEUE, head);
			// readable = [le32 stream_id][pcm frames...]
			let pcm = data.get(4..).unwrap_or(&[]);
			// A full ring means nobody is taking the audio; drop the oldest so
			// the host keeps the FRESHEST quarter second and playback timing
			// is never held hostage to a listener.
			if self.ring.len() + pcm.len() > RING_CAP {
				let over = self.ring.len() + pcm.len() - RING_CAP;
				let drop = over.min(self.ring.len());
				self.ring.drain(..drop);
				self.dropped += drop as u64;
			}
			let take = pcm.len().min(RING_CAP);
			self.ring.extend(pcm[pcm.len() - take..].iter().copied());
			// struct virtio_snd_pcm_status { le32 status; le32 latency_bytes }
			let mut st = status_bytes(S_OK);
			st.extend_from_slice(&(self.ring.len() as u32).to_le_bytes());
			let written = write_out(memory, &writable, &st);
			self.push_used(memory, TX_QUEUE, head, written);
		}
	}

	// ---- mmio ------------------------------------------------------------

	pub fn load(&mut self, address: u64) -> u8 {
		let off = address - BASE;
		match off {
			0x000 => 0x76, // "virt"
			0x001 => 0x69,
			0x002 => 0x72,
			0x003 => 0x74,
			0x004 => 2, // version 2 (non-legacy)
			0x008 => 25, // device id: sound
			0x00c => 0x51, // "QEMU"
			0x00d => 0x45,
			0x00e => 0x4d,
			0x00f => 0x55,
			0x010..=0x013 => {
				let sh = (self.device_features_sel as u64) * 32 + (off - 0x010) * 8;
				((VIRTIO_F_VERSION_1 >> sh) & 0xff) as u8
			}
			0x034 => MAX_QUEUE_SIZE as u8,
			0x035 => (MAX_QUEUE_SIZE >> 8) as u8,
			0x036 => (MAX_QUEUE_SIZE >> 16) as u8,
			0x037 => (MAX_QUEUE_SIZE >> 24) as u8,
			0x044 => self.queue().ready as u8,
			0x045..=0x047 => 0,
			0x060 => self.interrupt_status as u8,
			0x061 => (self.interrupt_status >> 8) as u8,
			0x062 => (self.interrupt_status >> 16) as u8,
			0x063 => (self.interrupt_status >> 24) as u8,
			0x070 => self.status as u8,
			0x071 => (self.status >> 8) as u8,
			0x072 => (self.status >> 16) as u8,
			0x073 => (self.status >> 24) as u8,
			0x0fc..=0x0ff => 0,
			// virtio_snd_config: jacks, streams, chmaps — one of each
			0x100..=0x1ff => {
				let cfg = off - 0x100;
				match cfg {
					0 | 4 | 8 => 1,
					1..=3 | 5..=7 | 9..=11 => 0,
					_ => 0,
				}
			}
			_ => 0,
		}
	}

	pub fn store(&mut self, address: u64, value: u8) {
		let off = address - BASE;
		let v = value as u32;
		match off {
			0x014..=0x017 => set_byte32(&mut self.device_features_sel, off - 0x014, v),
			0x020..=0x023 => {
				let sh = (self.driver_features_sel as u64) * 32 + (off - 0x020) * 8;
				self.driver_features =
					(self.driver_features & !(0xffu64 << sh)) | ((value as u64) << sh);
			}
			0x024..=0x027 => set_byte32(&mut self.driver_features_sel, off - 0x024, v),
			0x030..=0x033 => set_byte32(&mut self.queue_select, off - 0x030, v),
			0x038..=0x03b => {
				let mut n = self.queue().num;
				set_byte32(&mut n, off - 0x038, v);
				self.queue_mut().num = n;
			}
			0x044 => self.queue_mut().ready = (value & 1) == 1,
			// QueueNotify: tick() drains whatever is posted, so a notify needs
			// no per-queue action beyond being accepted.
			0x050..=0x053 => {}
			0x064 => {
				if (value & 0x1) == 1 {
					self.interrupt_status &= !0x1;
				}
			}
			0x070..=0x073 => {
				let mut s = self.status;
				set_byte32(&mut s, off - 0x070, v);
				self.status = s;
				if self.status == 0 {
					self.reset();
				}
			}
			0x080..=0x083 => { let mut a = self.queue().desc; set_byte64(&mut a, off - 0x080, value); self.queue_mut().desc = a; }
			0x084..=0x087 => { let mut a = self.queue().desc; set_byte64(&mut a, off - 0x084 + 4, value); self.queue_mut().desc = a; }
			0x090..=0x093 => { let mut a = self.queue().driver; set_byte64(&mut a, off - 0x090, value); self.queue_mut().driver = a; }
			0x094..=0x097 => { let mut a = self.queue().driver; set_byte64(&mut a, off - 0x094 + 4, value); self.queue_mut().driver = a; }
			0x0a0..=0x0a3 => { let mut a = self.queue().device; set_byte64(&mut a, off - 0x0a0, value); self.queue_mut().device = a; }
			0x0a4..=0x0a7 => { let mut a = self.queue().device; set_byte64(&mut a, off - 0x0a4 + 4, value); self.queue_mut().device = a; }
			_ => {}
		}
	}

	fn reset(&mut self) {
		self.queues = [Queue::new(), Queue::new(), Queue::new(), Queue::new()];
		self.interrupt_status = 0;
		self.driver_features = 0;
		self.ring.clear();
		self.running = false;
	}

	fn queue(&self) -> &Queue {
		&self.queues[(self.queue_select as usize) % NUM_QUEUES]
	}
	fn queue_mut(&mut self) -> &mut Queue {
		&mut self.queues[(self.queue_select as usize) % NUM_QUEUES]
	}

	fn avail_index(&self, memory: &mut MemoryWrapper, qi: usize) -> u16 {
		memory.read_halfword(self.queues[qi].driver.wrapping_add(2))
	}

	/// The next posted head WITHOUT consuming it — playback needs to know how
	/// big a buffer is before deciding it can afford to complete it.
	fn peek_avail(&mut self, memory: &mut MemoryWrapper, qi: usize) -> Option<u64> {
		if !self.queues[qi].is_ready() {
			return None;
		}
		if self.avail_index(memory, qi) == self.queues[qi].avail_cursor {
			return None;
		}
		let q = &self.queues[qi];
		let slot = (q.avail_cursor as u64) % (q.num as u64);
		let head = memory.read_halfword(q.driver.wrapping_add(4).wrapping_add(slot * 2));
		Some((head as u64) % (q.num as u64))
	}

	fn pop_avail(&mut self, memory: &mut MemoryWrapper, qi: usize) -> Option<u64> {
		if !self.queues[qi].is_ready() {
			return None;
		}
		if self.avail_index(memory, qi) == self.queues[qi].avail_cursor {
			return None;
		}
		let q = &self.queues[qi];
		let slot = (q.avail_cursor as u64) % (q.num as u64);
		let head = memory.read_halfword(q.driver.wrapping_add(4).wrapping_add(slot * 2));
		self.queues[qi].avail_cursor = self.queues[qi].avail_cursor.wrapping_add(1);
		Some((head as u64) % (self.queues[qi].num as u64))
	}

	fn push_used(&mut self, memory: &mut MemoryWrapper, qi: usize, head: u64, len: u32) {
		let q = &self.queues[qi];
		let used = q.device;
		let slot = (q.used_index as u64) % (q.num as u64);
		memory.write_word(used.wrapping_add(4).wrapping_add(slot * 8), head as u32);
		memory.write_word(used.wrapping_add(4).wrapping_add(slot * 8).wrapping_add(4), len);
		let next = q.used_index.wrapping_add(1);
		self.queues[qi].used_index = next;
		memory.write_halfword(used.wrapping_add(2), next);
		self.interrupt_status |= 0x1;
	}

	/// Total readable bytes in a chain, without copying any of them: the
	/// descriptors are 16 bytes each and only their lengths are needed.
	fn readable_len(&self, memory: &mut MemoryWrapper, qi: usize, head: u64) -> u64 {
		let q = &self.queues[qi];
		let queue_size = q.num as u64;
		let mut total = 0u64;
		let mut desc_index = head;
		for _ in 0..queue_size {
			let desc = q.desc + 16 * desc_index;
			let len = memory.read_word(desc.wrapping_add(8));
			let flags = memory.read_halfword(desc.wrapping_add(12));
			let next = (memory.read_halfword(desc.wrapping_add(14)) as u64) % queue_size;
			if (flags & VIRTQ_DESC_F_WRITE) == 0 {
				total += len as u64;
			}
			if (flags & VIRTQ_DESC_F_NEXT) == 0 {
				break;
			}
			desc_index = next;
		}
		total
	}

	fn walk_chain(&self, memory: &mut MemoryWrapper, qi: usize, head: u64)
		-> (Vec<u8>, Vec<(u64, u32)>) {
		let q = &self.queues[qi];
		let desc_base = q.desc;
		let queue_size = q.num as u64;
		let mut readable = Vec::new();
		let mut writable = Vec::new();
		let mut desc_index = head;
		for _ in 0..queue_size {
			let desc = desc_base + 16 * desc_index;
			let addr = memory.read_doubleword(desc);
			let len = memory.read_word(desc.wrapping_add(8));
			let flags = memory.read_halfword(desc.wrapping_add(12));
			let next = (memory.read_halfword(desc.wrapping_add(14)) as u64) % queue_size;
			match (flags & VIRTQ_DESC_F_WRITE) != 0 {
				true => writable.push((addr, len)),
				false => {
					for i in 0..len as u64 {
						readable.push(memory.read_byte(addr + i));
					}
				}
			}
			if (flags & VIRTQ_DESC_F_NEXT) == 0 {
				break;
			}
			desc_index = next;
		}
		(readable, writable)
	}
}

fn status_bytes(status: u32) -> Vec<u8> {
	status.to_le_bytes().to_vec()
}

fn le32(buf: &[u8], at: usize) -> u32 {
	let mut v = [0u8; 4];
	for i in 0..4 {
		v[i] = buf.get(at + i).copied().unwrap_or(0);
	}
	u32::from_le_bytes(v)
}

/// Spill `bytes` across the chain's writable descriptors in order, stopping
/// when either runs out. Returns how much landed, which is what the used ring
/// reports as the response length.
fn write_out(memory: &mut MemoryWrapper, writable: &[(u64, u32)], bytes: &[u8]) -> u32 {
	let mut written = 0usize;
	for &(addr, len) in writable {
		if written >= bytes.len() {
			break;
		}
		let n = (len as usize).min(bytes.len() - written);
		for i in 0..n {
			memory.write_byte(addr + i as u64, bytes[written + i]);
		}
		written += n;
	}
	written as u32
}

fn set_byte32(reg: &mut u32, pos: u64, value: u32) {
	let sh = pos * 8;
	*reg = (*reg & !(0xffu32 << sh)) | ((value & 0xff) << sh);
}

fn set_byte64(reg: &mut u64, pos: u64, value: u8) {
	let sh = pos * 8;
	*reg = (*reg & !(0xffu64 << sh)) | ((value as u64) << sh);
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every take is a whole number of frames. A chunk that starts mid-frame
	/// swaps the channels for its whole length in any consumer that assumes
	/// alignment — the music mixer and the Opus encoder both do.
	#[test]
	fn takes_are_frame_aligned() {
		let mut snd = VirtioSnd::new();
		snd.channels = 2; // 4 bytes per frame
		for i in 0..30u8 {
			snd.ring.push_back(i);
		}
		// An odd cap must still cut on a frame boundary.
		let a = snd.take_pcm(7);
		assert_eq!(a.len() % 4, 0, "cap 7 gave {} bytes", a.len());
		assert_eq!(a.len(), 4);
		// And a cap past the end takes only whole frames of what is there.
		let b = snd.take_pcm(1000);
		assert_eq!(b.len() % 4, 0, "drain gave {} bytes", b.len());
		assert_eq!(b.len(), 24, "26 bytes left -> 6 whole frames");
	}

	/// The card must play at exactly its byte rate however finely the device
	/// is serviced. This is the regression that made DOOM chop: the emulator
	/// services devices every 32 retired instructions, so `accrue` is called
	/// with an mtime delta of about 3, and the old truncating division paid
	/// out ZERO bytes on nearly every one of those three million calls a
	/// second. A card that plays at a fraction of real time backs the whole
	/// pipeline up behind it.
	#[test]
	fn pays_full_rate_when_serviced_finely() {
		for delta in [1u64, 3, 7, 227, 1000] {
			let mut snd = VirtioSnd::new();
			snd.rate_hz = 11_025;
			snd.channels = 2;
			snd.running = true;
			snd.last_mtime = 1;
			let mut paid = 0u64;
			let mut t = 1u64;
			while t < 1 + MTIME_HZ {
				t += delta;
				snd.accrue(t);
				// Spend as a playing stream would, so the pause cap (which
				// exists for a stopped stream) never enters into it.
				paid += snd.credit;
				snd.credit = 0;
			}
			let want = 11_025u64 * 2 * 2;
			assert!(
				paid + delta >= want && paid <= want + delta,
				"servicing every {} mtime ticks paid {} bytes for one second, want {}",
				delta, paid, want
			);
		}
	}
}
