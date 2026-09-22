use terminal::Terminal;

const IER_RXINT_BIT: u8 = 0x1;
const IER_THREINT_BIT: u8 = 0x2;

const IIR_THR_EMPTY: u8 = 0x2;
const IIR_RD_AVAILABLE: u8 = 0x4;
const IIR_NO_INTERRUPT: u8 = 0x7;

const LSR_DATA_AVAILABLE: u8 = 0x1;
const LSR_THR_EMPTY: u8 = 0x20;

// risc-box patch: did the clock cross a multiple of `period` when it advanced
// from `previous` to `now`? Replaces `clock % period == 0`, which silently
// stops firing once the clock advances in steps larger than one.
/// How often, in retired instructions, an empty receive register takes the next typed byte.
///
/// Upstream used 0x38400 (230,400), with the comment "just an arbitary number @TODO: Fix me".
/// Nothing depends on the value except typing speed: RBR must be EMPTY before a byte is taken, so
/// the guest's own reads are the backpressure and no cadence can overrun it. At the ~100 MIPS of a
/// native host 230,400 was 2 ms a byte and nobody noticed; inside a VBS enclave, where the
/// emulator is Pulley bytecode and the guest runs ~0.55 MIPS, it is 0.42 s a byte - a 56-byte
/// command took 23 s to reach the shell before it ran at all. Devices are serviced every 32
/// instructions, so 1024 still costs one terminal poll per thousand instructions, and a byte now
/// waits at most that long plus the guest's own interrupt handling.
pub(crate) const RX_POLL_PERIOD: u64 = 0x400;

fn crossed(previous: u64, now: u64, period: u64) -> bool {
	(now / period) != (previous / period)
}

/// Emulates UART. Refer to the [specification](http://www.ti.com/lit/ug/sprugp1/sprugp1.pdf)
/// for the detail.
pub struct Uart {
	clock: u64,
	rbr: u8, // receiver buffer register
	thr: u8, // transmitter holding register
	ier: u8, // interrupt enable register
	iir: u8, // interrupt identification register
	lcr: u8, // line control register
	mcr: u8, // modem control register
	lsr: u8, // line status register
	scr: u8, // scratch,
	thre_ip: bool,
	interrupting: bool,
	terminal: Box<dyn Terminal>
}

impl Uart {
	/// Creates a new `Uart`. Input/Output data is transferred via `Terminal`.
	pub fn new(terminal: Box<dyn Terminal>) -> Self {
		Uart {
			clock: 0,
			rbr: 0,
			thr: 0,
			ier: 0,
			iir: 0,
			lcr: 0,
			mcr: 0,
			lsr: LSR_THR_EMPTY,
			scr: 0,
			thre_ip: false,
			interrupting: false,
			terminal: terminal
		}
	}

	/// Runs one cycle. `Uart` gets/puts input/output data via `Terminal`
	/// at certain timing.
	// risc-box patch: `n` = instructions retired since the last service. The
	// two cadences below used to test for an exact multiple of the clock,
	// which only holds while the clock advances one at a time; the clock now
	// moves in steps (see Cpu::tick), so they test for CROSSING a multiple
	// instead. Same average cadence, and a step can no longer jump over it.
	pub fn tick(&mut self, n: u64) {
		let previous_clock = self.clock;
		self.clock = self.clock.wrapping_add(n);
		let mut rx_ip = false;

		// Reads input.
		// risc-box patch: RX_POLL_PERIOD (was upstream's "arbitrary" 0x38400).
		if crossed(previous_clock, self.clock, RX_POLL_PERIOD) && self.rbr == 0 {
			let value = self.terminal.get_input();
			if value != 0 {
				self.rbr = value;
				self.lsr |= LSR_DATA_AVAILABLE;
				self.update_iir();
				if (self.ier & IER_RXINT_BIT) != 0 {
					rx_ip = true;
				}
			}
		}

		// risc-box patch: output used to be consumed here, one byte per
		// service, with LSR_THR_EMPTY only returning at that moment. That
		// serialized every console byte behind a service interval plus an
		// interrupt round trip, and a driver whose bounded LSR wait expired
		// first would overwrite the pending byte — at coarser service
		// intervals the console visibly dropped characters. The terminal is
		// host-buffered, so THR is consumed at the store now (see store());
		// this tick only delivers the pending THRE edge at service cadence.

		// risc-box patch: the receive side is LEVEL-triggered, not an edge
		// on the tick that pulled the byte. The PLIC pending bit is cleared
		// by the guest's claim-complete, and the CPU's SEIP by delivery; a
		// byte that lands after the serial ISR's last LSR read but before
		// that completion used to raise its edge into an already-pending
		// line and then have it wiped by the completion — after which the
		// byte sat in RBR (blocking every byte behind it, since RBR must be
		// empty to pull the next), no edge could ever come, and a guest
		// parked in WFI slept forever with a full receive register. Seen
		// as a resumed machine going silent 17 characters into a pasted
		// line. Level semantics need no memory of edges: while data is
		// unread and the RX interrupt is enabled, the line is high and the
		// PLIC re-arms it whenever the guest has completed the previous
		// one (see Plic::tick). THRE stays a one-shot.
		let rx_level = (self.ier & IER_RXINT_BIT) != 0 && self.rbr != 0;
		let _ = rx_ip;
		if self.thre_ip || rx_level {
			self.interrupting = true;
			self.thre_ip = false;
		} else {
			self.interrupting = false;
		}
	}

	/// Indicates whether an interrupt happens in the current cycle.
	/// Note: This behavior is for easily handling an interrupt as
	/// "Edge-triggered" from `Uart` module user.
	/// It doesn't seem to be mentioned in the UART specification
	/// whether interrupt should be "Edge-triggered" or "Level-triggered" but
	/// we implement it as "Edge-triggered" so far because it would support more
	/// drivers. I speculate some drivers assume "Edge-triggered" interrupt
	/// while drivers rarely rely on the behavior of "Level-triggered" interrupt
	/// which keeps interrupting while interrupt pending signal is asserted.
	pub fn is_interrupting(&self) -> bool {
		self.interrupting
	}

	fn update_iir(&mut self) {
		let rx_ip = (self.ier & IER_RXINT_BIT) != 0 && self.rbr != 0;
		let thre_ip = (self.ier & IER_THREINT_BIT) != 0 && self.thr == 0;

		// Which should be prioritized RX interrupt or THRE interrupt?
		if rx_ip {
			self.iir = IIR_RD_AVAILABLE;
		} else if thre_ip {
			self.iir = IIR_THR_EMPTY;
		} else {
			self.iir = IIR_NO_INTERRUPT;
		}
	}

	/// Loads register content
	///
	/// # Arguments
	/// * `address`
	pub fn load(&mut self, address: u64) -> u8 {
		//println!("UART Load AD:{:X}", address);
		match address {
			0x10000000 => match (self.lcr >> 7) == 0 {
				true => {
					let rbr = self.rbr;
					self.rbr = 0;
					self.lsr &= !LSR_DATA_AVAILABLE;
					self.update_iir();
					rbr
				},
				false => 0 // @TODO: Implement properly
			},
			0x10000001 => match (self.lcr >> 7) == 0 {
				true => self.ier,
				false => 0 // @TODO: Implement properly
			},
			0x10000002 => self.iir,
			0x10000003 => self.lcr,
			0x10000004 => self.mcr,
			0x10000005 => self.lsr,
			0x10000007 => self.scr,
			_ => 0
		}
	}

	/// Stores register content
	///
	/// # Arguments
	/// * `address`
	/// * `value`
	pub fn store(&mut self, address: u64, value: u8) {
		//println!("UART Store AD:{:X} VAL:{:X}", address, value);
		match address {
			// Transfer Holding Register
			// risc-box patch: consumed immediately (the terminal buffers);
			// THR is never busy, so no write can ever overwrite a pending
			// byte. The THRE interrupt is armed here and delivered at the
			// next device service, which is when the PLIC looks anyway.
			0x10000000 => match (self.lcr >> 7) == 0 {
				true => {
					self.terminal.put_byte(value);
					self.thr = 0;
					self.lsr |= LSR_THR_EMPTY;
					self.update_iir();
					if (self.ier & IER_THREINT_BIT) != 0 {
						self.thre_ip = true;
					}
				},
				false => {} // @TODO: Implement properly
			},
			0x10000001 => match (self.lcr >> 7) == 0 {
				true => {
					// This bahavior isn't written in the data sheet
					// but some drivers seem to rely on it.
					if (self.ier & IER_THREINT_BIT) == 0 &&
						(value & IER_THREINT_BIT) != 0 &&
						self.thr == 0 {
						self.thre_ip = true;
					}
					self.ier = value;
					self.update_iir();
				},
				false => {} // @TODO: Implement properly
			},
			0x10000003 => {
				self.lcr = value;
			},
			0x10000004 => {
				self.mcr = value;
			},
			0x10000007 => {
				self.scr = value;
			},
			_ => {}
		};
	}

	/// Returns mutable reference to `Terminal`.
	pub fn get_mut_terminal(&mut self) -> &mut Box<dyn Terminal> {
		&mut self.terminal
	}
}

// risc-box patch (snapshot): see src/snapshot.rs. The terminal is the
// host's and is not part of the machine; a restored UART talks to whatever
// terminal the fresh emulator was built with.
use snapshot::{De, Ser};

impl Uart {
	pub fn snapshot(&self, w: &mut Ser) {
		w.u64(self.clock);
		w.u8(self.rbr);
		w.u8(self.thr);
		w.u8(self.ier);
		w.u8(self.iir);
		w.u8(self.lcr);
		w.u8(self.mcr);
		w.u8(self.lsr);
		w.u8(self.scr);
		w.bool(self.thre_ip);
		w.bool(self.interrupting);
	}

	pub fn restore(&mut self, r: &mut De) -> Result<(), String> {
		self.clock = r.u64()?;
		self.rbr = r.u8()?;
		self.thr = r.u8()?;
		self.ier = r.u8()?;
		self.iir = r.u8()?;
		self.lcr = r.u8()?;
		self.mcr = r.u8()?;
		self.lsr = r.u8()?;
		self.scr = r.u8()?;
		self.thre_ip = r.bool()?;
		self.interrupting = r.bool()?;
		Ok(())
	}
}

#[cfg(test)]
mod rx_cadence_tests {
	use super::*;
	use std::cell::RefCell;
	use std::collections::VecDeque;
	use std::rc::Rc;

	/// A terminal whose typed input is a queue the test can inspect.
	struct Queue(Rc<RefCell<VecDeque<u8>>>);
	impl Terminal for Queue {
		fn put_byte(&mut self, _value: u8) {}
		fn get_output(&mut self) -> u8 { 0 }
		fn put_input(&mut self, data: u8) { self.0.borrow_mut().push_back(data); }
		fn get_input(&mut self) -> u8 { self.0.borrow_mut().pop_front().unwrap_or(0) }
	}

	fn uart_with(input: &[u8]) -> (Uart, Rc<RefCell<VecDeque<u8>>>) {
		let q = Rc::new(RefCell::new(input.iter().copied().collect::<VecDeque<u8>>()));
		(Uart::new(Box::new(Queue(q.clone()))), q)
	}

	/// Typed bytes reach a guest that is reading within one poll period each - the device is
	/// serviced every 32 instructions, so that is the step here.
	#[test]
	fn typed_bytes_arrive_within_a_poll_period() {
		let (mut uart, _) = uart_with(b"uname\n");
		let (mut got, mut insns) = (Vec::new(), 0u64);
		while got.len() < 6 && insns < 100 * RX_POLL_PERIOD {
			uart.tick(32);
			insns += 32;
			if uart.load(0x10000005) & LSR_DATA_AVAILABLE != 0 {
				got.push(uart.load(0x10000000));
			}
		}
		assert_eq!(got, b"uname\n");
		assert!(insns <= 6 * RX_POLL_PERIOD, "6 bytes took {insns} instructions");
		// the old cadence, for the record: 6 x 230,400
		assert!(insns * 100 < 6 * 0x38400, "no faster than before ({insns})");
	}

	/// A byte the guest has NOT read is never overwritten: the receive register is the only buffer,
	/// and the queue behind it waits however fast the poll runs.
	#[test]
	fn an_unread_byte_is_never_overwritten() {
		let (mut uart, q) = uart_with(b"ab");
		for _ in 0..(10 * RX_POLL_PERIOD / 32) { uart.tick(32); }
		assert_eq!(q.borrow().len(), 1, "exactly one byte taken while the guest was not reading");
		assert_eq!(uart.load(0x10000000), b'a');
		for _ in 0..(2 * RX_POLL_PERIOD / 32) { uart.tick(32); }
		assert_eq!(uart.load(0x10000000), b'b');
		assert!(q.borrow().is_empty());
	}
}
