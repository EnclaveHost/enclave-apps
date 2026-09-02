// risc-box patch: Virtio Input device — a virtual HID the host injects
// pointer/keyboard events into. Mapped at 0x10003000, IRQ 3 on the PLIC.
//
// Unlike the block/net devices here, virtio-input is a MODERN (virtio 1.0)
// device: the Linux `virtio_input` driver hard-requires VIRTIO_F_VERSION_1
// (drivers/virtio/virtio_input.c) and the virtio core refuses to bind it to a
// legacy transport ("device must provide VIRTIO_F_VERSION_1"). So this is a
// version-2 virtio-mmio device: it offers VERSION_1, runs the FEATURES_OK
// handshake, and uses the split-virtqueue address registers (QueueDesc/
// QueueDriver/QueueDevice, 64-bit each) instead of the legacy single PFN.
// The ring FORMATS are identical to the legacy device — only where the three
// rings live changes — so the descriptor/avail/used walking mirrors
// virtio_net.rs; just the base addresses come from driver-programmed
// registers rather than PFN * page_size.
//
// The device presents ONE combined HID:
//   EV_ABS  ABS_X/ABS_Y (0..32767)     absolute pointer (INPUT_PROP_POINTER)
//   EV_KEY  BTN_LEFT/RIGHT/MIDDLE      mouse buttons
//           + keyboard keys 1..247     a full keyboard
//   EV_REL  REL_WHEEL/REL_HWHEEL       scroll
// libinput fans the ABS+BTN capability into a cursor and the KEY_* codes into
// a keyboard, so one device gives the session both.
//
// Based on VIRTIO v1.1: section 5.8 (Input Device) + section 4.2.2/4.2.3
// (MMIO transport, non-legacy register layout).

use std::collections::VecDeque;

use mmu::MemoryWrapper;

const BASE: u64 = 0x10003000;
const MAX_QUEUE_SIZE: u32 = 256;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// Device status bits (VIRTIO 2.1)
const STATUS_DRIVER_OK: u32 = 4;

// VIRTIO_F_VERSION_1 is feature bit 32.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const EVENT_QUEUE: u32 = 0;
const STATUS_QUEUE: u32 = 1;

// virtio-input config selects (5.8.4)
const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

// Linux input-event-codes.h subset we advertise
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const INPUT_PROP_POINTER: u16 = 0x00; // "this absolute device is a pointer"

const ABS_MAX_VALUE: i32 = 32767; // the absolute coordinate space we expose

const DEVICE_NAME: &[u8] = b"risc-box virtual input";

/// A single Linux input event, host-injected, drained into the eventq.
#[derive(Clone, Copy)]
pub struct InputEvent {
	pub kind: u16,
	pub code: u16,
	pub value: u32,
}

/// Per-virtqueue state, modern split layout: the driver programs the three
/// ring addresses independently and flips QueueReady.
struct Queue {
	num: u32,
	ready: bool,
	desc: u64,   // descriptor table base
	driver: u64, // avail ring base (a.k.a. "driver area")
	device: u64, // used ring base (a.k.a. "device area")
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

pub struct VirtioInput {
	device_features_sel: u32,
	driver_features: u64,
	driver_features_sel: u32,
	queue_select: u32,
	interrupt_status: u32,
	status: u32,
	queues: [Queue; 2],
	// config-space cursor the driver writes (select/subsel) before reading
	cfg_select: u8,
	cfg_subsel: u8,
	// host-injected events awaiting a posted eventq buffer
	pending: VecDeque<InputEvent>,
}

impl VirtioInput {
	pub fn new() -> Self {
		VirtioInput {
			device_features_sel: 0,
			driver_features: 0,
			driver_features_sel: 0,
			queue_select: 0,
			interrupt_status: 0,
			status: 0,
			queues: [Queue::new(), Queue::new()],
			cfg_select: 0,
			cfg_subsel: 0,
			pending: VecDeque::new(),
		}
	}

	/// Host → device: queue one input event. Callers push a burst of
	/// (type,code,value) events and terminate it with EV_SYN/SYN_REPORT so the
	/// guest input core dispatches the group atomically.
	pub fn push_event(&mut self, kind: u16, code: u16, value: u32) {
		const CAP: usize = 4096;
		// risc-box patch: when this overflowed it used to drop the OLDEST
		// event, which for input is the worst thing to drop. Key events come
		// in pairs, and losing a release leaves that key held down in the
		// guest forever — which does not present as a lost keystroke, it
		// presents as a key repeating endlessly.
		//
		// Pointer motion is safe to drop instead: this is an ABSOLUTE device
		// (INPUT_PROP_POINTER, ABS_X/ABS_Y), so a discarded position is
		// corrected by the very next one. Scroll deltas are droppable for the
		// same practical reason. So on overflow, evict the oldest motion or
		// scroll event and let every key and button transition through. Only
		// if the backlog is somehow ALL key traffic does this fall back to
		// dropping the oldest, which at 4096 events should not happen.
		if self.pending.len() >= CAP {
			let victim = self
				.pending
				.iter()
				.position(|e| e.kind == EV_ABS || e.kind == EV_REL);
			match victim {
				Some(i) => { self.pending.remove(i); },
				None => { self.pending.pop_front(); },
			}
		}
		self.pending.push_back(InputEvent { kind, code, value });
	}

	/// The absolute coordinate space (0..MAX on both axes) callers map into.
	pub fn abs_max() -> i32 {
		ABS_MAX_VALUE
	}

	pub fn is_interrupting(&mut self) -> bool {
		(self.interrupt_status & 0x1) == 1
	}

	/// Runs one cycle: drain pending host events into posted eventq buffers.
	pub fn tick(&mut self, memory: &mut MemoryWrapper) {
		if !self.driver_ready() {
			return;
		}
		while !self.pending.is_empty() {
			let Some(head) = self.pop_avail(memory, EVENT_QUEUE as usize) else { break };
			let ev = self.pending.pop_front().expect("pending non-empty");
			// struct virtio_input_event { __le16 type; __le16 code; __le32 value; }
			let mut buf = [0u8; 8];
			buf[0..2].copy_from_slice(&ev.kind.to_le_bytes());
			buf[2..4].copy_from_slice(&ev.code.to_le_bytes());
			buf[4..8].copy_from_slice(&ev.value.to_le_bytes());
			let (_readable, writable) = self.walk_chain(memory, EVENT_QUEUE as usize, head);
			let mut written = 0usize;
			for (addr, len) in writable {
				if written >= buf.len() {
					break;
				}
				let n = std::cmp::min(len as usize, buf.len() - written);
				for i in 0..n {
					memory.write_byte(addr + i as u64, buf[written + i]);
				}
				written += n;
			}
			self.push_used(memory, EVENT_QUEUE as usize, head, written as u32);
		}
	}

	fn driver_ready(&self) -> bool {
		(self.status & STATUS_DRIVER_OK) != 0 && self.queues[EVENT_QUEUE as usize].is_ready()
	}

	// ---- config space (device-specific, at BASE+0x100) ---------------------

	fn config_query(&self) -> (u8, Vec<u8>) {
		match self.cfg_select {
			CFG_ID_NAME => (DEVICE_NAME.len() as u8, DEVICE_NAME.to_vec()),
			CFG_ID_SERIAL => (0, vec![]),
			CFG_ID_DEVIDS => {
				let mut u = Vec::new();
				u.extend_from_slice(&0x06u16.to_le_bytes()); // bustype BUS_VIRTUAL
				u.extend_from_slice(&0x1af4u16.to_le_bytes()); // vendor
				u.extend_from_slice(&0x0001u16.to_le_bytes()); // product
				u.extend_from_slice(&0x0001u16.to_le_bytes()); // version
				(8, u)
			}
			CFG_PROP_BITS => (1, vec![1u8 << INPUT_PROP_POINTER]),
			CFG_EV_BITS => self.ev_bits(self.cfg_subsel as u16),
			CFG_ABS_INFO => match self.cfg_subsel as u16 {
				ABS_X | ABS_Y => {
					let mut u = Vec::new();
					u.extend_from_slice(&0i32.to_le_bytes()); // min
					u.extend_from_slice(&ABS_MAX_VALUE.to_le_bytes()); // max
					u.extend_from_slice(&0i32.to_le_bytes()); // fuzz
					u.extend_from_slice(&0i32.to_le_bytes()); // flat
					u.extend_from_slice(&0i32.to_le_bytes()); // res
					(20, u)
				}
				_ => (0, vec![]),
			},
			_ => (0, vec![]),
		}
	}

	fn ev_bits(&self, ev_type: u16) -> (u8, Vec<u8>) {
		let set = |bits: &mut Vec<u8>, code: u16| {
			let byte = (code / 8) as usize;
			if byte >= bits.len() {
				bits.resize(byte + 1, 0);
			}
			bits[byte] |= 1 << (code % 8);
		};
		match ev_type {
			t if t == EV_SYN => {
				let mut b = Vec::new();
				set(&mut b, EV_KEY);
				set(&mut b, EV_REL);
				set(&mut b, EV_ABS);
				(b.len() as u8, b)
			}
			t if t == EV_KEY => {
				let mut b = Vec::new();
				for k in 1u16..=247 {
					set(&mut b, k);
				}
				set(&mut b, 0x110); // BTN_LEFT
				set(&mut b, 0x111); // BTN_RIGHT
				set(&mut b, 0x112); // BTN_MIDDLE
				set(&mut b, 0x14a); // BTN_TOUCH
				(b.len() as u8, b)
			}
			t if t == EV_REL => {
				let mut b = Vec::new();
				set(&mut b, REL_HWHEEL);
				set(&mut b, REL_WHEEL);
				(b.len() as u8, b)
			}
			t if t == EV_ABS => {
				let mut b = Vec::new();
				set(&mut b, ABS_X);
				set(&mut b, ABS_Y);
				(b.len() as u8, b)
			}
			_ => (0, vec![]),
		}
	}

	fn config_byte(&self, offset: u64) -> u8 {
		match offset {
			0x00 => self.cfg_select,
			0x01 => self.cfg_subsel,
			0x02 => self.config_query().0, // size
			0x03..=0x07 => 0,              // reserved
			0x08..=0x87 => {
				let (_size, u) = self.config_query();
				*u.get((offset - 0x08) as usize).unwrap_or(&0)
			}
			_ => 0,
		}
	}

	// ---- modern virtio-mmio register file ----------------------------------
	// The transport reads/writes 32-bit registers; the MMU calls us per byte,
	// so each register is spread over its 4 byte offsets (little-endian).

	pub fn load(&mut self, address: u64) -> u8 {
		let off = address - BASE;
		match off {
			// Magic "virt"
			0x000 => 0x76,
			0x001 => 0x69,
			0x002 => 0x72,
			0x003 => 0x74,
			// Version: 2 (non-legacy) — REQUIRED for virtio_input
			0x004 => 2,
			// Device id: 18 (input)
			0x008 => 18,
			// Vendor id "QEMU"
			0x00c => 0x51,
			0x00d => 0x45,
			0x00e => 0x4d,
			0x00f => 0x55,
			// DeviceFeatures window (offers VIRTIO_F_VERSION_1 in the high dword)
			0x010..=0x013 => {
				let sh = (self.device_features_sel as u64) * 32 + (off - 0x010) * 8;
				((VIRTIO_F_VERSION_1 >> sh) & 0xff) as u8
			}
			// QueueNumMax
			0x034 => MAX_QUEUE_SIZE as u8,
			0x035 => (MAX_QUEUE_SIZE >> 8) as u8,
			0x036 => (MAX_QUEUE_SIZE >> 16) as u8,
			0x037 => (MAX_QUEUE_SIZE >> 24) as u8,
			// QueueReady
			0x044 => self.queue().ready as u8,
			0x045..=0x047 => 0,
			// InterruptStatus
			0x060 => self.interrupt_status as u8,
			0x061 => (self.interrupt_status >> 8) as u8,
			0x062 => (self.interrupt_status >> 16) as u8,
			0x063 => (self.interrupt_status >> 24) as u8,
			// Status
			0x070 => self.status as u8,
			0x071 => (self.status >> 8) as u8,
			0x072 => (self.status >> 16) as u8,
			0x073 => (self.status >> 24) as u8,
			// ConfigGeneration — constant (the input config only changes in
			// response to the driver's own select/subsel writes, never mid-read)
			0x0fc..=0x0ff => 0,
			// Device-specific config space
			0x100..=0x1ff => self.config_byte(off - 0x100),
			_ => 0,
		}
	}

	pub fn store(&mut self, address: u64, value: u8) {
		let off = address - BASE;
		let v = value as u32;
		match off {
			0x014..=0x017 => set_byte32(&mut self.device_features_sel, off - 0x014, v),
			// DriverFeatures (windowed by DriverFeaturesSel) — accepted as-is;
			// the driver sets VERSION_1, which is all we offer
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
			// QueueNotify — the driver poked a queue
			0x050..=0x053 => {
				if off == 0x050 {
					// low byte carries the queue index for our small indices
					if v == STATUS_QUEUE {
						// statusq (LED etc.): nothing to act on; buffers are
						// left for a future tick, harmless for an LED-less HID
					}
					// eventq notify needs no action — tick() drains pending
					// events into whatever buffers are posted
				}
			}
			// InterruptACK
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
			// Split-virtqueue addresses (64-bit, low then high)
			0x080..=0x083 => { let mut a = self.queue().desc; set_byte64(&mut a, off - 0x080, value); self.queue_mut().desc = a; }
			0x084..=0x087 => { let mut a = self.queue().desc; set_byte64(&mut a, off - 0x084 + 4, value); self.queue_mut().desc = a; }
			0x090..=0x093 => { let mut a = self.queue().driver; set_byte64(&mut a, off - 0x090, value); self.queue_mut().driver = a; }
			0x094..=0x097 => { let mut a = self.queue().driver; set_byte64(&mut a, off - 0x094 + 4, value); self.queue_mut().driver = a; }
			0x0a0..=0x0a3 => { let mut a = self.queue().device; set_byte64(&mut a, off - 0x0a0, value); self.queue_mut().device = a; }
			0x0a4..=0x0a7 => { let mut a = self.queue().device; set_byte64(&mut a, off - 0x0a4 + 4, value); self.queue_mut().device = a; }
			// Device-specific config: select/subsel writes drive config reads
			0x100 => self.cfg_select = value,
			0x101 => self.cfg_subsel = value,
			_ => {}
		}
	}

	fn reset(&mut self) {
		self.queues = [Queue::new(), Queue::new()];
		self.interrupt_status = 0;
		self.driver_features = 0;
		self.pending.clear();
	}

	fn queue(&self) -> &Queue {
		&self.queues[(self.queue_select & 1) as usize]
	}
	fn queue_mut(&mut self) -> &mut Queue {
		&mut self.queues[(self.queue_select & 1) as usize]
	}

	fn avail_index(&self, memory: &mut MemoryWrapper, qi: usize) -> u16 {
		memory.read_halfword(self.queues[qi].driver.wrapping_add(2))
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

/// Splice one byte into a u32 register at byte position `pos` (0..=3).
fn set_byte32(reg: &mut u32, pos: u64, value: u32) {
	let sh = pos * 8;
	*reg = (*reg & !(0xffu32 << sh)) | ((value & 0xff) << sh);
}

/// Splice one byte into a u64 register at byte position `pos` (0..=7).
fn set_byte64(reg: &mut u64, pos: u64, value: u8) {
	let sh = pos * 8;
	*reg = (*reg & !(0xffu64 << sh)) | ((value as u64) << sh);
}

// risc-box patch: the overflow policy is a correctness property, not a
// preference — a dropped key RELEASE leaves that key held down in the guest
// for good, which presents as a key repeating rather than as a lost keystroke.
#[cfg(test)]
mod tests {
	use super::*;

	fn queue_len(d: &VirtioInput) -> usize {
		d.pending.len()
	}

	#[test]
	fn overflow_evicts_motion_and_keeps_keys() {
		let mut d = VirtioInput::new();
		// Fill well past capacity with pointer motion, then send a key press
		// and its release. Both must survive.
		for i in 0..8192 {
			d.push_event(EV_ABS, 0, i as u32);
		}
		d.push_event(EV_KEY, 30, 1); // KEY_A down
		d.push_event(EV_KEY, 30, 0); // KEY_A up
		let keys: Vec<_> = d.pending.iter().filter(|e| e.kind == EV_KEY).collect();
		assert_eq!(keys.len(), 2, "key transitions must not be evicted by motion");
		assert_eq!(keys[0].value, 1);
		assert_eq!(keys[1].value, 0, "the RELEASE is the one that must never be lost");
		assert!(queue_len(&d) <= 4097, "queue must stay bounded");
	}

	#[test]
	fn key_pairs_survive_a_flood_between_them() {
		let mut d = VirtioInput::new();
		d.push_event(EV_KEY, 42, 1); // shift down
		for i in 0..8192 {
			d.push_event(EV_ABS, 1, i as u32);
		}
		d.push_event(EV_KEY, 42, 0); // shift up
		let shift: Vec<_> = d.pending.iter().filter(|e| e.kind == EV_KEY).collect();
		assert_eq!(shift.len(), 2, "a held modifier must still get its release");
	}
}

// risc-box patch (snapshot): see src/snapshot.rs.
use snapshot::{De, Ser};

impl VirtioInput {
	pub fn snapshot(&self, w: &mut Ser) {
		w.u32(self.device_features_sel);
		w.u64(self.driver_features);
		w.u32(self.driver_features_sel);
		w.u32(self.queue_select);
		w.u32(self.interrupt_status);
		w.u32(self.status);
		for q in &self.queues {
			w.u32(q.num);
			w.bool(q.ready);
			w.u64(q.desc);
			w.u64(q.driver);
			w.u64(q.device);
			w.u16(q.avail_cursor);
			w.u16(q.used_index);
		}
		w.u8(self.cfg_select);
		w.u8(self.cfg_subsel);
		w.u32(self.pending.len() as u32);
		for e in &self.pending {
			w.u16(e.kind);
			w.u16(e.code);
			w.u32(e.value);
		}
	}

	pub fn restore(&mut self, r: &mut De) -> Result<(), String> {
		self.device_features_sel = r.u32()?;
		self.driver_features = r.u64()?;
		self.driver_features_sel = r.u32()?;
		self.queue_select = r.u32()?;
		self.interrupt_status = r.u32()?;
		self.status = r.u32()?;
		for q in self.queues.iter_mut() {
			q.num = r.u32()?;
			q.ready = r.bool()?;
			q.desc = r.u64()?;
			q.driver = r.u64()?;
			q.device = r.u64()?;
			q.avail_cursor = r.u16()?;
			q.used_index = r.u16()?;
		}
		self.cfg_select = r.u8()?;
		self.cfg_subsel = r.u8()?;
		let n = r.u32()? as usize;
		if n > 4096 {
			return Err(format!("snapshot: virtio-input has {} pending events", n));
		}
		self.pending.clear();
		for _ in 0..n {
			let kind = r.u16()?;
			let code = r.u16()?;
			let value = r.u32()?;
			self.pending.push_back(InputEvent { kind, code, value });
		}
		Ok(())
	}
}
