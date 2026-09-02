use cpu::MIP_SEIP;

// Based on SiFive Interrupt Cookbook
// https://sifive.cdn.prismic.io/sifive/0d163928-2128-42be-a75a-464df65e04e0_sifive-interrupt-cookbook.pdf

/// Emulates PLIC known as Interrupt Controller.
/// Refer to the [specification](https://sifive.cdn.prismic.io/sifive%2Fc89f6e5a-cf9e-44c3-a3db-04420702dcc1_sifive+e31+manual+v19.08.pdf)
/// for the detail.
pub struct Plic {
	clock: u64,
	irq: u32,
	enabled: u64,
	threshold: u32,
	ips: [u8; 1024],
	priorities: [u32; 1024],
	needs_update_irq: bool
}

// @TODO: IRQ numbers should be configurable with device tree
const VIRTIO_IRQ: u32 = 1;
const NET_IRQ: u32 = 2; // risc-box patch: virtio-net at 0x10002000
const INPUT_IRQ: u32 = 3; // risc-box patch: virtio-input at 0x10003000
const SND_IRQ: u32 = 4; // risc-box patch: virtio-snd at 0x10004000
const GPU_IRQ: u32 = 5; // risc-box patch: virtio-gpu at 0x10005000
const UART_IRQ: u32 = 10;

impl Plic {
	/// Creates a new `Plic`.
	pub fn new() -> Self {
		Plic {
			clock: 0,
			irq: 0,
			enabled: 0,
			threshold: 0,
			priorities: [0; 1024],
			ips: [0; 1024],
			needs_update_irq: false
		}
	}

	/// Runs one cycle. Takes interrupting signals from devices and
	/// raises an interrupt to CPU depending on configuration.
	/// If interrupt occurs a certain bit of `mip` regiser is risen
	/// depending on interrupt type.
	///
	/// # Arguments
	/// * `virtio_ip`
	/// * `uart_ip`
	/// * `mip`
	// risc-box patch: `n` = instructions retired since the last service.
	pub fn tick(&mut self, n: u64, virtio_ip: bool, net_ip: bool, input_ip: bool, snd_ip: bool, gpu_ip: bool, uart_ip: bool, mip: &mut u64) {
		self.clock = self.clock.wrapping_add(n);

		// risc-box patch: the virtio lines are LEVEL-triggered, and the PLIC
		// used to edge-detect them through a cached copy sampled once per
		// device-service tick. That loses interrupts: the guest's ACK
		// (level drops) and the device's next raise (level returns) can both
		// happen BETWEEN two samples, the cache sees high==high, and no
		// interrupt is ever posted again — after which the ring exhausts,
		// the level sticks high, and the device is dead for the life of the
		// guest. Batched device servicing made that window big enough for a
		// human moving a mouse to hit it in seconds (the frozen-cursor bug,
		// 2026-08-16). Proper level semantics need no cache: re-arm whenever
		// the line is high and the pending bit is clear (i.e. the guest has
		// claimed/completed the previous one). A spurious re-run of an ISR
		// that finds nothing new is the worst this can produce, and that is
		// how level-triggered lines behave everywhere.
		if virtio_ip && !self.ip_bit(VIRTIO_IRQ) {
			self.set_ip(VIRTIO_IRQ);
		}
		if net_ip && !self.ip_bit(NET_IRQ) {
			self.set_ip(NET_IRQ);
		}
		if input_ip && !self.ip_bit(INPUT_IRQ) {
			self.set_ip(INPUT_IRQ);
		}
		if snd_ip && !self.ip_bit(SND_IRQ) {
			self.set_ip(SND_IRQ);
		}
		if gpu_ip && !self.ip_bit(GPU_IRQ) {
			self.set_ip(GPU_IRQ);
		}

		// risc-box patch: the UART line is level-triggered like the virtio
		// ones (see uart.rs tick for the lost-byte failure this closes): a
		// high line re-arms the pending bit once the guest has completed
		// the previous claim. THRE is still delivered as the one tick the
		// UART reports it, which this form carries exactly as before.
		if uart_ip && !self.ip_bit(UART_IRQ) {
			self.set_ip(UART_IRQ);
		}

		if self.needs_update_irq {
			self.update_irq(mip);
			self.needs_update_irq = false;
		}
	}

	fn update_irq(&mut self, mip: &mut u64) {
		// Hardcoded VirtIO and UART
		// @TODO: Should be configurable with device tree

		let virtio_ip = ((self.ips[(VIRTIO_IRQ >> 3) as usize] >> (VIRTIO_IRQ & 7)) & 1) == 1;
		let net_ip = ((self.ips[(NET_IRQ >> 3) as usize] >> (NET_IRQ & 7)) & 1) == 1; // risc-box patch
		let input_ip = ((self.ips[(INPUT_IRQ >> 3) as usize] >> (INPUT_IRQ & 7)) & 1) == 1; // risc-box patch
		let snd_ip = ((self.ips[(SND_IRQ >> 3) as usize] >> (SND_IRQ & 7)) & 1) == 1; // risc-box patch
		let gpu_ip = ((self.ips[(GPU_IRQ >> 3) as usize] >> (GPU_IRQ & 7)) & 1) == 1; // risc-box patch
		let uart_ip = ((self.ips[(UART_IRQ >> 3) as usize] >> (UART_IRQ & 7)) & 1) == 1;

		// Which should be prioritized, virtio or uart?

		let virtio_priority = self.priorities[VIRTIO_IRQ as usize];
		let net_priority = self.priorities[NET_IRQ as usize]; // risc-box patch
		let input_priority = self.priorities[INPUT_IRQ as usize]; // risc-box patch
		let snd_priority = self.priorities[SND_IRQ as usize]; // risc-box patch
		let gpu_priority = self.priorities[GPU_IRQ as usize]; // risc-box patch
		let uart_priority = self.priorities[UART_IRQ as usize];

		let virtio_enabled = ((self.enabled >> VIRTIO_IRQ) & 1) == 1;
		let net_enabled = ((self.enabled >> NET_IRQ) & 1) == 1; // risc-box patch
		let input_enabled = ((self.enabled >> INPUT_IRQ) & 1) == 1; // risc-box patch
		let snd_enabled = ((self.enabled >> SND_IRQ) & 1) == 1; // risc-box patch
		let gpu_enabled = ((self.enabled >> GPU_IRQ) & 1) == 1; // risc-box patch
		let uart_enabled = ((self.enabled >> UART_IRQ) & 1) == 1;

		// risc-box patch: every line the PLIC can raise must appear HERE, not
		// just in the set_ip path above. A device missing from this table has
		// its pending bit set and never selected, so the guest driver waits on
		// a completion that is sitting in the used ring — which is exactly how
		// virtio-snd first presented ("control message timeout", probe -110).
		let ips = [virtio_ip, net_ip, input_ip, snd_ip, gpu_ip, uart_ip];
		let enables = [virtio_enabled, net_enabled, input_enabled, snd_enabled, gpu_enabled, uart_enabled];
		let priorities = [virtio_priority, net_priority, input_priority, snd_priority, gpu_priority, uart_priority];
		let irqs = [VIRTIO_IRQ, NET_IRQ, INPUT_IRQ, SND_IRQ, GPU_IRQ, UART_IRQ];

		let mut irq = 0;
		let mut priority = 0;
		for i in 0..irqs.len() {
			if ips[i] && enables[i] &&
				priorities[i] > self.threshold &&
				priorities[i] > priority {
					irq = irqs[i];
					priority = priorities[i];
			}
		}

		self.irq = irq;
		if self.irq != 0 {
			//println!("IRQ: {:X}", self.irq);
			*mip |= MIP_SEIP;
		}
	}

	fn ip_bit(&self, irq: u32) -> bool {
		((self.ips[(irq >> 3) as usize] >> (irq & 7)) & 1) == 1
	}

	fn set_ip(&mut self, irq: u32) {
		let index = (irq >> 3) as usize;
		self.ips[index] = self.ips[index] | (1 << (irq & 7));
		self.needs_update_irq = true;
	}

	fn clear_ip(&mut self, irq: u32) {
		let index = (irq >> 3) as usize;
		// `irq & 7`, not `irq`: shifting a u8 by 10 (the UART line) is a
		// masked shift in release — which happened to land on the right bit —
		// and a panic in debug. Say what is meant.
		self.ips[index] = self.ips[index] & !(1 << (irq & 7));
		self.needs_update_irq = true;
	}

	/// Loads register content
	///
	/// # Arguments
	/// * `address`
	pub fn load(&self, address: u64) -> u8 {
		//println!("PLIC Load AD:{:X}", address);
		match address {
			0x0c000000..=0x0c000fff => {
				let offset = address % 4;
				let index = ((address - 0xc000000) >> 2) as usize;
				let pos = offset << 3;
				(self.priorities[index] >> pos) as u8
			},
			0x0c001000..=0x0c00107f => {
				let index = (address - 0xc001000) as usize;
				self.ips[index]
			},
			0x0c002080 => self.enabled as u8,
			0x0c002081 => (self.enabled >> 8) as u8,
			0x0c002082 => (self.enabled >> 16) as u8,
			0x0c002083 => (self.enabled >> 24) as u8,
			0x0c002084 => (self.enabled >> 32) as u8,
			0x0c002085 => (self.enabled >> 40) as u8,
			0x0c002086 => (self.enabled >> 48) as u8,
			0x0c002087 => (self.enabled >> 56) as u8,
			0x0c201000 => self.threshold as u8,
			0x0c201001 => (self.threshold >> 8) as u8,
			0x0c201002 => (self.threshold >> 16) as u8,
			0x0c201003 => (self.threshold >> 24) as u8,
			0x0c201004 => self.irq as u8,
			0x0c201005 => (self.irq >> 8) as u8,
			0x0c201006 => (self.irq >> 16) as u8,
			0x0c201007 => (self.irq >> 24) as u8,
			_ => 0
		}
	}

	/// Stores register content
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn store(&mut self, address: u64, value: u8) {
		//println!("PLIC Store AD:{:X} VAL:{:X}", address, value);
		match address {
			0x0c000000..=0x0c000fff => {
				let offset = address % 4;
				let index = ((address - 0xc000000) >> 2) as usize;
				let pos = offset << 3;
				self.priorities[index] = (self.priorities[index] & !(0xff << pos)) | ((value as u32) << pos);
				self.needs_update_irq = true;
			},
			// Enable. Only first 64 interrupt sources support so far.
			// @TODO: Implement all 1024 interrupt source enables.
			0x0c002080 => {
				self.enabled = (self.enabled & !0xff) | (value as u64);
				self.needs_update_irq = true;
			},
			0x0c002081 => {
				self.enabled = (self.enabled & !(0xff << 8)) | ((value as u64) << 8);
			},
			0x0c002082 => {
				self.enabled = (self.enabled & !(0xff << 16)) | ((value as u64) << 16);
			},
			0x0c002083 => {
				self.enabled = (self.enabled & !(0xff << 24)) | ((value as u64) << 24);
			},
			0x0c002084 => {
				self.enabled = (self.enabled & !(0xff << 32)) | ((value as u64) << 32);
			},
			0x0c002085 => {
				self.enabled = (self.enabled & !(0xff << 40)) | ((value as u64) << 40);
			},
			0x0c002086 => {
				self.enabled = (self.enabled & !(0xff << 48)) | ((value as u64) << 48);
			},
			0x0c002087 => {
				self.enabled = (self.enabled & !(0xff << 56)) | ((value as u64) << 56);
			},
			0x0c201000 => {
				self.threshold = (self.threshold & !0xff) | (value as u32);
				self.needs_update_irq = true;
			},
			0x0c201001 => {
				self.threshold = (self.threshold & !(0xff << 8)) | ((value as u32) << 8);
			},
			0x0c201002 => {
				self.threshold = (self.threshold & !(0xff << 16)) | ((value as u32) << 16);
			},
			0x0c201003 => {
				self.threshold = (self.threshold & !(0xff << 24)) | ((value as u32) << 24);
			},
			// Claim
			0x0c201004 => {
				// Assuming written data is a byte so far
				// @TODO: Should be four bytes.
				self.clear_ip(value as u32);
			},
			_ => {}
		};
	}
}

// risc-box patch (snapshot): see src/snapshot.rs.
use snapshot::{De, Ser};

impl Plic {
	pub fn snapshot(&self, w: &mut Ser) {
		w.u64(self.clock);
		w.u32(self.irq);
		w.u64(self.enabled);
		w.u32(self.threshold);
		w.raw(&self.ips);
		for p in self.priorities.iter() {
			w.u32(*p);
		}
		w.bool(self.needs_update_irq);
	}

	pub fn restore(&mut self, r: &mut De) -> Result<(), String> {
		self.clock = r.u64()?;
		self.irq = r.u32()?;
		self.enabled = r.u64()?;
		self.threshold = r.u32()?;
		let n = self.ips.len();
		let ips = r.take(n)?;
		self.ips.copy_from_slice(ips);
		for i in 0..self.priorities.len() {
			self.priorities[i] = r.u32()?;
		}
		self.needs_update_irq = r.bool()?;
		Ok(())
	}
}
