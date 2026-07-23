// risc-box patch: Virtio Network device (legacy MMIO, device id 1), the
// counterpart of `virtio_block_disk.rs`. Mapped at 0x10002000, IRQ 2 on the
// PLIC. Ethernet frames move to/from the embedding application through a
// pluggable `NetBackend` (see `net.rs`), the same shape as the UART's
// `Terminal`.
//
// Based on Virtual I/O Device (VIRTIO) Version 1.1, section 5.1 (Network
// Device) + section 4.2.4 (Legacy interface).
// https://docs.oasis-open.org/virtio/virtio/v1.1/csprd01/virtio-v1.1-csprd01.html

use mmu::MemoryWrapper;
use net::{NetBackend, NullNetBackend};

const MAX_QUEUE_SIZE: u64 = 256;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// Feature bit: the device reports a stable MAC in config space.
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

// Legacy virtio-net header (no MRG_RXBUF negotiated): 10 bytes, all zero for
// a device that offers no checksum/GSO offloads.
const NET_HDR_LEN: u64 = 10;

// Device status bit written by the driver when it is ready to drive queues.
const STATUS_DRIVER_OK: u32 = 4;

// A fixed locally-administered MAC; the guest reads it via config space
// because VIRTIO_NET_F_MAC is offered.
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

/// Per-virtqueue state (legacy PFN-based layout, like the block device, but
/// the net device drives two queues so it is held per queue).
struct Queue {
	size: u32,
	align: u32,
	pfn: u32,
	avail_cursor: u16, // next avail ring entry this device will consume
	used_index: u16    // published used ring index
}

impl Queue {
	fn new() -> Self {
		Queue { size: 0, align: 0x1000, pfn: 0, avail_cursor: 0, used_index: 0 }
	}

	fn ready(&self) -> bool {
		self.pfn != 0 && self.size != 0
	}

	fn desc_base(&self, page_size: u64) -> u64 {
		self.pfn as u64 * page_size
	}

	fn avail_base(&self, page_size: u64) -> u64 {
		self.desc_base(page_size) + self.size as u64 * 16
	}

	fn used_base(&self, page_size: u64) -> u64 {
		let align = self.align as u64;
		((self.avail_base(page_size) + 4 + self.size as u64 * 2 + align - 1) / align) * align
	}
}

pub struct VirtioNet {
	device_features: u64,
	device_features_sel: u32,
	driver_features: u32,
	guest_page_size: u32,
	queue_select: u32,
	queue_notify: u32,
	interrupt_status: u32,
	status: u32,
	queues: [Queue; 2],
	tx_notified: bool,
	pending_rx: Option<Vec<u8>>,
	backend: Box<dyn NetBackend>
}

impl VirtioNet {
	pub fn new() -> Self {
		VirtioNet {
			device_features: VIRTIO_NET_F_MAC,
			device_features_sel: 0,
			driver_features: 0,
			guest_page_size: 0,
			queue_select: 0,
			queue_notify: 0,
			interrupt_status: 0,
			status: 0,
			queues: [Queue::new(), Queue::new()],
			tx_notified: false,
			pending_rx: None,
			backend: Box::new(NullNetBackend::new())
		}
	}

	/// Attaches the application's network backend (replacing the default
	/// frame-dropping one). Expected to be called once, before boot.
	pub fn set_backend(&mut self, backend: Box<dyn NetBackend>) {
		self.backend = backend;
	}

	pub fn get_mut_backend(&mut self) -> &mut Box<dyn NetBackend> {
		&mut self.backend
	}

	/// Indicates whether `VirtioNet` raises an interrupt signal. Level
	/// triggered: stays high until the driver acks via the MMIO register.
	pub fn is_interrupting(&mut self) -> bool {
		(self.interrupt_status & 0x1) == 1
	}

	/// Runs one cycle: drains guest transmissions after a queue notify and
	/// delivers at most one pending host frame into posted receive buffers.
	pub fn tick(&mut self, memory: &mut MemoryWrapper) {
		if self.tx_notified {
			self.tx_notified = false;
			self.handle_tx(memory);
		}
		if self.driver_ready() {
			if self.pending_rx.is_none() {
				self.pending_rx = self.backend.guest_rx();
			}
			if self.pending_rx.is_some() {
				self.handle_rx(memory);
			}
		}
	}

	fn driver_ready(&self) -> bool {
		(self.status & STATUS_DRIVER_OK) != 0 && self.queues[RX_QUEUE].ready()
	}

	pub fn load(&mut self, address: u64) -> u8 {
		match address {
			// Magic number: 0x74726976 ("virt")
			0x10002000 => 0x76,
			0x10002001 => 0x69,
			0x10002002 => 0x72,
			0x10002003 => 0x74,
			// Device version: 1 (Legacy device)
			0x10002004 => 1,
			// Virtio Subsystem Device id: 1 (Network device)
			0x10002008 => 1,
			// Virtio Subsystem Vendor id: 0x554d4551
			0x1000200c => 0x51,
			0x1000200d => 0x45,
			0x1000200e => 0x4d,
			0x1000200f => 0x55,
			// Device features window
			0x10002010 => ((self.device_features >> (self.device_features_sel * 32)) & 0xff) as u8,
			0x10002011 => (((self.device_features >> (self.device_features_sel * 32)) >> 8) & 0xff) as u8,
			0x10002012 => (((self.device_features >> (self.device_features_sel * 32)) >> 16) & 0xff) as u8,
			0x10002013 => (((self.device_features >> (self.device_features_sel * 32)) >> 24) & 0xff) as u8,
			// Maximum virtual queue size
			0x10002034 => MAX_QUEUE_SIZE as u8,
			0x10002035 => (MAX_QUEUE_SIZE >> 8) as u8,
			0x10002036 => (MAX_QUEUE_SIZE >> 16) as u8,
			0x10002037 => (MAX_QUEUE_SIZE >> 24) as u8,
			// Guest physical page number of the selected queue
			0x10002040 => self.queue().pfn as u8,
			0x10002041 => (self.queue().pfn >> 8) as u8,
			0x10002042 => (self.queue().pfn >> 16) as u8,
			0x10002043 => (self.queue().pfn >> 24) as u8,
			// Interrupt status
			0x10002060 => self.interrupt_status as u8,
			0x10002061 => (self.interrupt_status >> 8) as u8,
			0x10002062 => (self.interrupt_status >> 16) as u8,
			0x10002063 => (self.interrupt_status >> 24) as u8,
			// Device status
			0x10002070 => self.status as u8,
			0x10002071 => (self.status >> 8) as u8,
			0x10002072 => (self.status >> 16) as u8,
			0x10002073 => (self.status >> 24) as u8,
			// Config space: MAC address (VIRTIO_NET_F_MAC), then status u16
			0x10002100..=0x10002105 => GUEST_MAC[(address - 0x10002100) as usize],
			0x10002106 => 1, // VIRTIO_NET_S_LINK_UP (only read if F_STATUS; harmless)
			0x10002107 => 0,
			_ => 0
		}
	}

	pub fn store(&mut self, address: u64, value: u8) {
		match address {
			0x10002014..=0x10002017 => {
				let shift = (address - 0x10002014) * 8;
				self.device_features_sel =
					(self.device_features_sel & !(0xff << shift)) | ((value as u32) << shift);
			},
			0x10002020..=0x10002023 => {
				let shift = (address - 0x10002020) * 8;
				self.driver_features =
					(self.driver_features & !(0xff << shift)) | ((value as u32) << shift);
			},
			0x10002028..=0x1000202b => {
				let shift = (address - 0x10002028) * 8;
				self.guest_page_size =
					(self.guest_page_size & !(0xff << shift)) | ((value as u32) << shift);
			},
			0x10002030..=0x10002033 => {
				let shift = (address - 0x10002030) * 8;
				self.queue_select =
					(self.queue_select & !(0xff << shift)) | ((value as u32) << shift);
				if address == 0x10002033 && self.queue_select > 1 {
					panic!("VirtioNet: queue {} is not supported (rx=0/tx=1 only).", self.queue_select);
				}
			},
			0x10002038..=0x1000203b => {
				let shift = (address - 0x10002038) * 8;
				let q = self.queue_mut();
				q.size = (q.size & !(0xff << shift)) | ((value as u32) << shift);
			},
			0x1000203c..=0x1000203f => {
				let shift = (address - 0x1000203c) * 8;
				let q = self.queue_mut();
				q.align = (q.align & !(0xff << shift)) | ((value as u32) << shift);
			},
			0x10002040..=0x10002043 => {
				let shift = (address - 0x10002040) * 8;
				let q = self.queue_mut();
				q.pfn = (q.pfn & !(0xff << shift)) | ((value as u32) << shift);
			},
			0x10002050..=0x10002053 => {
				let shift = (address - 0x10002050) * 8;
				self.queue_notify =
					(self.queue_notify & !(0xff << shift)) | ((value as u32) << shift);
				if address == 0x10002053 {
					// RX notifies (buffers replenished) need no action: tick()
					// finds posted buffers when a frame is pending.
					if self.queue_notify == TX_QUEUE as u32 {
						self.tx_notified = true;
					}
				}
			},
			0x10002064 => {
				if (value & 0x1) == 1 {
					self.interrupt_status &= !0x1;
				}
			},
			0x10002070..=0x10002073 => {
				let shift = (address - 0x10002070) * 8;
				self.status = (self.status & !(0xff << shift)) | ((value as u32) << shift);
			},
			_ => {}
		};
	}

	fn queue(&self) -> &Queue {
		&self.queues[(self.queue_select & 1) as usize]
	}

	fn queue_mut(&mut self) -> &mut Queue {
		&mut self.queues[(self.queue_select & 1) as usize]
	}

	fn avail_index(&self, memory: &mut MemoryWrapper, qi: usize) -> u16 {
		let page_size = self.guest_page_size as u64;
		memory.read_halfword(self.queues[qi].avail_base(page_size).wrapping_add(2))
	}

	/// Pops the next available descriptor chain head, or None if the driver
	/// has not posted anything new on this queue.
	fn pop_avail(&mut self, memory: &mut MemoryWrapper, qi: usize) -> Option<u64> {
		if self.avail_index(memory, qi) == self.queues[qi].avail_cursor {
			return None;
		}
		let page_size = self.guest_page_size as u64;
		let q = &self.queues[qi];
		let slot = (q.avail_cursor as u64) % (q.size as u64);
		let head = memory.read_halfword(q.avail_base(page_size).wrapping_add(4).wrapping_add(slot * 2));
		self.queues[qi].avail_cursor = self.queues[qi].avail_cursor.wrapping_add(1);
		Some((head as u64) % (self.queues[qi].size as u64))
	}

	/// Publishes a used-ring entry and raises the interrupt.
	fn push_used(&mut self, memory: &mut MemoryWrapper, qi: usize, head: u64, len: u32) {
		let page_size = self.guest_page_size as u64;
		let q = &self.queues[qi];
		let used = q.used_base(page_size);
		let slot = (q.used_index as u64) % (q.size as u64);
		memory.write_word(used.wrapping_add(4).wrapping_add(slot * 8), head as u32);
		memory.write_word(used.wrapping_add(4).wrapping_add(slot * 8).wrapping_add(4), len);
		let next = q.used_index.wrapping_add(1);
		self.queues[qi].used_index = next;
		memory.write_halfword(used.wrapping_add(2), next);
		self.interrupt_status |= 0x1;
	}

	/// Walks one descriptor chain, returning (readable bytes concatenated,
	/// writable descriptors as (addr, len) in order).
	fn walk_chain(&self, memory: &mut MemoryWrapper, qi: usize, head: u64)
		-> (Vec<u8>, Vec<(u64, u32)>) {
		let page_size = self.guest_page_size as u64;
		let q = &self.queues[qi];
		let desc_base = q.desc_base(page_size);
		let queue_size = q.size as u64;
		let mut readable = Vec::new();
		let mut writable = Vec::new();
		let mut desc_index = head;
		// A chain can't be longer than the ring; bound the walk so a corrupt
		// next-pointer loop can't hang the emulator.
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

	/// Guest → host: consume every posted transmit chain. Each chain is the
	/// 10-byte legacy net header followed by one ethernet frame.
	fn handle_tx(&mut self, memory: &mut MemoryWrapper) {
		if !self.queues[TX_QUEUE].ready() {
			return;
		}
		while let Some(head) = self.pop_avail(memory, TX_QUEUE) {
			let (readable, _writable) = self.walk_chain(memory, TX_QUEUE, head);
			if readable.len() as u64 > NET_HDR_LEN {
				self.backend.guest_tx(readable[NET_HDR_LEN as usize..].to_vec());
			}
			self.push_used(memory, TX_QUEUE, head, 0);
		}
	}

	/// Host → guest: deliver the pending frame into the next posted receive
	/// chain (10-byte zeroed legacy header + frame across its writable
	/// descriptors). Leaves the frame pending if no buffer is posted yet.
	fn handle_rx(&mut self, memory: &mut MemoryWrapper) {
		let Some(head) = self.pop_avail(memory, RX_QUEUE) else {
			return;
		};
		let frame = self.pending_rx.take().expect("handle_rx called with a frame pending");
		let (_readable, writable) = self.walk_chain(memory, RX_QUEUE, head);
		let mut payload = Vec::with_capacity(NET_HDR_LEN as usize + frame.len());
		payload.extend_from_slice(&[0u8; NET_HDR_LEN as usize]);
		payload.extend_from_slice(&frame);
		let mut written = 0usize;
		for (addr, len) in writable {
			if written >= payload.len() {
				break;
			}
			let n = std::cmp::min(len as usize, payload.len() - written);
			for i in 0..n {
				memory.write_byte(addr + i as u64, payload[written + i]);
			}
			written += n;
		}
		// A frame that outgrows the posted buffers is truncated; with no GSO
		// offloads offered the driver posts full-MTU buffers, so this only
		// guards against a malformed driver.
		self.push_used(memory, RX_QUEUE, head, written as u32);
	}
}
