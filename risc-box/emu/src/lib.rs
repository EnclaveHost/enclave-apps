// @TODO: temporal
const TEST_MEMORY_CAPACITY: u64 = 1024 * 512;
// risc-box patch: 512 MiB (was 128). A desktop guest needs it — Xorg's
// System()/fork of xkbcomp during keyboard init fails silently under 128 MiB
// (heuristic overcommit refuses to commit the forked address space of the
// large X process, so the child never runs and X aborts). The DTB's
// `memory@80000000` size cell is synced to the allocation at init (mmu.rs);
// the framebuffer (0x87e00000, reserved) stays inside.
const PROGRAM_MEMORY_CAPACITY: u64 = 1024 * 1024 * 512; // default; see setup_ram_bytes()
// Guest RAM is overridable per machine via setup_ram_bytes() (risc-box wires it to the
// deployment config's `ramMiB`; 512 MiB default). Ceilings, measured 2026-08-13: RAM is
// ONE contiguous Vec and Rust caps a single allocation at isize::MAX (2 GiB on wasm32) —
// 2.5 GiB panics `capacity overflow` on any engine, so the usable max is just under 2 GiB.
// Separately, TOTAL linear memory (this Vec + the fs image Vec + overhead) needs a
// wasmtime 49+ engine: 47 refuses growth past ~1.5 GiB total.

extern crate fnv;

use self::fnv::FnvHashMap;

pub mod cpu;
pub mod terminal;
pub mod net; // risc-box patch
pub mod default_terminal;
pub mod memory;
pub mod mmu;
pub mod elf_analyzer;
pub mod device;
#[cfg(feature = "jit")]
pub mod jit; // risc-box patch: PLATFORM-JIT.md translator (feature-gated)

use cpu::{Cpu, Xlen};
use elf_analyzer::{ElfAnalyzer};
use terminal::Terminal;

/// RISC-V emulator. It emulates RISC-V CPU and peripheral devices.
///
/// Sample code to run the emulator.
/// ```ignore
/// // Creates an emulator with arbitary terminal
/// let mut emulator = Emulator::new(Box::new(DefaultTerminal::new()));
/// // Set up program content binary
/// emulator.setup_program(program_content);
/// // Set up Filesystem content binary
/// emulator.setup_filesystem(fs_content);
/// // Go!
/// emulator.run();
/// ```
pub struct Emulator {
	cpu: Cpu,

	// risc-box patch: per-machine RAM override (bytes); None = PROGRAM_MEMORY_CAPACITY.
	ram_bytes: Option<u64>,

	/// Stores mapping from symbol to virtual address
	symbol_map: FnvHashMap::<String, u64>,

	/// [`riscv-tests`](https://github.com/riscv/riscv-tests) program specific
	/// properties. Whether the program set by `setup_program()` is
	/// [`riscv-tests`](https://github.com/riscv/riscv-tests) program.
	is_test: bool,

	/// [`riscv-tests`](https://github.com/riscv/riscv-tests) specific properties.
	/// The address where data will be sent to terminal
	tohost_addr: u64
}

impl Emulator {
	/// Creates a new `Emulator`. [`Terminal`](terminal/trait.Terminal.html)
	/// is internally used for transferring input/output data to/from `Emulator`.
	/// 
	/// # Arguments
	/// * `terminal`
	pub fn new(terminal: Box<dyn Terminal>) -> Self {
		Emulator {
			cpu: Cpu::new(terminal),

			ram_bytes: None,

			symbol_map: FnvHashMap::default(),

			// These can be updated in setup_program()
			is_test: false,
			tohost_addr: 0 // assuming tohost_addr is non-zero if exists
		}
	}

	/// Runs program set by `setup_program()`. Calls `run_test()` if the program
	/// is [`riscv-tests`](https://github.com/riscv/riscv-tests).
	/// Otherwise calls `run_program()`.
	pub fn run(&mut self) {
		match self.is_test {
			true => self.run_test(),
			false => self.run_program()
		};
	}

	/// Runs program set by `setup_program()`. The emulator won't stop forever.
	pub fn run_program(&mut self) {
		loop {
			self.tick();
		}
	}

	/// Method for running [`riscv-tests`](https://github.com/riscv/riscv-tests) program.
	/// The differences from `run_program()` are
	/// * Disassembles every instruction and dumps to terminal
	/// * The emulator stops when the test finishes
	/// * Displays the result message (pass/fail) to terminal
	pub fn run_test(&mut self) {
		// @TODO: Send this message to terminal?
		println!("This elf file seems riscv-tests elf file. Running in test mode.");
		loop {
			let disas = self.cpu.disassemble_next_instruction();
			self.put_bytes_to_terminal(disas.as_bytes());
			self.put_bytes_to_terminal(&[10]); // new line

			self.tick();

			// It seems in riscv-tests ends with end code
			// written to a certain physical memory address
			// (0x80001000 in mose test cases) so checking
			// the data in the address and terminating the test
			// if non-zero data is written.
			// End code 1 seems to mean pass.
			let endcode = self.cpu.get_mut_mmu().load_word_raw(self.tohost_addr);
			if endcode != 0 {
				match endcode {
					1 => {
						self.put_bytes_to_terminal(format!("Test Passed with {:X}\n", endcode).as_bytes())
					},
					_ => {
						self.put_bytes_to_terminal(format!("Test Failed with {:X}\n", endcode).as_bytes())
					}
				};
				break;
			}
		}
	}

	/// Helper method. Sends ascii code bytes to terminal.
	///
	/// # Arguments
	/// * `bytes`
	fn put_bytes_to_terminal(&mut self, bytes: &[u8]) {
		for i in 0..bytes.len() {
			self.cpu.get_mut_terminal().put_byte(bytes[i]);
		}
	}

	/// Runs CPU one cycle
	pub fn tick(&mut self) {
		self.cpu.tick();
	}

	/// risc-box patch: runs `n` instructions in one call — the per-call and
	/// per-instruction loop overhead is amortized inside Cpu::run, and a
	/// WFI-parked guest consumes the batch without spinning. Embedders'
	/// tick-batches should call this instead of tick() in a loop.
	pub fn run_n(&mut self, n: u64) {
		self.cpu.run(n);
	}

	/// risc-box patch (blockstats feature): coverage histogram dump.
	#[cfg(feature = "blockstats")]
	pub fn dump_block_stats(&mut self) {
		self.cpu.dump_block_stats();
	}

	/// Sets up program run by the program. This method analyzes the passed content
	/// and configure CPU properly. If the passed contend doesn't seem ELF file,
	/// it panics. This method is expected to be called only once.
	///
	/// # Arguments
	/// * `data` Program binary
	// @TODO: Make ElfAnalyzer and move the core logic there.
	// @TODO: Returns `Err` if the passed contend doesn't seem ELF file
	pub fn setup_program(&mut self, data: Vec<u8>) {
		let analyzer = ElfAnalyzer::new(data);

		if !analyzer.validate() {
			panic!("This file does not seem ELF file");
		}

		let header = analyzer.read_header();
		//let program_headers = analyzer._read_program_headers(&header);
		let section_headers = analyzer.read_section_headers(&header);

		let mut program_data_section_headers = vec![];
		let mut symbol_table_section_headers = vec![];
		let mut string_table_section_headers = vec![];

		for i in 0..section_headers.len() {
			match section_headers[i].sh_type {
				1 => program_data_section_headers.push(&section_headers[i]),
				2 => symbol_table_section_headers.push(&section_headers[i]),
				3 => string_table_section_headers.push(&section_headers[i]),
				_ => {}
			};
		}

		// Find program data section named .tohost to detect if the elf file is riscv-tests
		self.tohost_addr = match analyzer.find_tohost_addr(
			&program_data_section_headers,
			&string_table_section_headers) {
			Some(address) => address,
			None => 0
		};

		// Creates symbol - virtual address mapping
		if string_table_section_headers.len() > 0 {
			let entries = analyzer.read_symbol_entries(&header, &symbol_table_section_headers);
			// Assuming symbols are in the first string table section.
			// @TODO: What if symbol can be in the second or later string table sections?
			let map = analyzer.create_symbol_map(&entries, &string_table_section_headers[0]);
			for key in map.keys() {
				self.symbol_map.insert(key.to_string(), *map.get(key).unwrap());
			}
		}

		// Detected whether the elf file is riscv-tests.
		// Setting up CPU and Memory depending on it.

		self.cpu.update_xlen(match header.e_width {
			32 => Xlen::Bit32,
			64 => Xlen::Bit64,
			_ => panic!("No happen")
		});

		if self.tohost_addr != 0 {
			self.is_test = true;
			self.cpu.get_mut_mmu().init_memory(TEST_MEMORY_CAPACITY);
		} else {
			self.is_test = false;
			let ram = self.ram_bytes.unwrap_or(PROGRAM_MEMORY_CAPACITY);
			self.cpu.get_mut_mmu().init_memory(ram);
		}

		for i in 0..program_data_section_headers.len() {
			let sh_addr = program_data_section_headers[i].sh_addr;
			let sh_offset = program_data_section_headers[i].sh_offset as usize;
			let sh_size = program_data_section_headers[i].sh_size as usize;
			if sh_addr >= 0x80000000 && sh_offset > 0 && sh_size > 0 {
				for j in 0..sh_size {
					self.cpu.get_mut_mmu().store_raw(sh_addr + j as u64, analyzer.read_byte(sh_offset + j));
				}
			}
		}

		self.cpu.update_pc(header.e_entry);
	}

	/// Loads symbols of program and adds them to `symbol_map`.
	///
	/// # Arguments
	/// * `content` Program binary
	pub fn load_program_for_symbols(&mut self, content: Vec<u8>) {
		let analyzer = ElfAnalyzer::new(content);

		if !analyzer.validate() {
			panic!("This file does not seem ELF file");
		}

		let header = analyzer.read_header();
		let section_headers = analyzer.read_section_headers(&header);

		let mut program_data_section_headers = vec![];
		let mut symbol_table_section_headers = vec![];
		let mut string_table_section_headers = vec![];

		for i in 0..section_headers.len() {
			match section_headers[i].sh_type {
				1 => program_data_section_headers.push(&section_headers[i]),
				2 => symbol_table_section_headers.push(&section_headers[i]),
				3 => string_table_section_headers.push(&section_headers[i]),
				_ => {}
			};
		}

		// Creates symbol - virtual address mapping
		if string_table_section_headers.len() > 0 {
			let entries = analyzer.read_symbol_entries(&header, &symbol_table_section_headers);
			// Assuming symbols are in the first string table section.
			// @TODO: What if symbol can be in the second or later string table sections?
			let map = analyzer.create_symbol_map(&entries, &string_table_section_headers[0]);
			for key in map.keys() {
				self.symbol_map.insert(key.to_string(), *map.get(key).unwrap());
			}
		}
	}

	/// Sets up filesystem. Use this method if program (e.g. Linux) uses
	/// filesystem. This method is expected to be called up to only once.
	///
	/// # Arguments
	/// * `content` File system content binary
	pub fn setup_filesystem(&mut self, content: Vec<u8>) {
		self.cpu.get_mut_mmu().init_disk(content);
	}

	/// Sets up device tree. The emulator has default device tree configuration.
	/// If you want to override it, use this method. This method is expected to
	/// to be called up to only once.
	///
	/// # Arguments
	/// * `content` DTB content binary
	/// risc-box patch: sets guest RAM size in bytes. Call BEFORE setup_program()
	/// (which allocates the memory). The DTB memory@80000000 node is patched to
	/// match when memory is initialized, so the kernel and the Vec agree.
	pub fn setup_ram_bytes(&mut self, bytes: u64) {
		self.ram_bytes = Some(bytes);
	}

	/// risc-box patch: the guest's clock, in its own 10 MHz ticks — what
	/// `rdtime` returns. Compared against the host's elapsed time it says
	/// directly whether the guest is living faster or slower than the world.
	pub fn guest_mtime(&self) -> u64 {
		self.cpu.get_mmu().get_clint().read_mtime()
	}

	/// risc-box patch: set the simple-framebuffer's resolution (see
	/// `Mmu::set_dtb_framebuffer`). Call before boot; returns false if the size
	/// was rejected, in which case the default 1024x768 still stands.
	pub fn set_framebuffer_size(&mut self, width: u32, height: u32) -> bool {
		self.cpu.get_mut_mmu().set_dtb_framebuffer(width, height)
	}

	/// risc-box patch: run the guest's clock off the host's monotonic clock
	/// rather than off retired instructions (see `Clint::set_wall_clock`).
	/// Call before the guest boots: the kernel reads the timebase once.
	pub fn set_wall_clock(&mut self, on: bool) {
		self.cpu.get_mut_mmu().get_mut_clint().set_wall_clock(on);
	}

	pub fn setup_dtb(&mut self, content: Vec<u8>) {
		self.cpu.get_mut_mmu().init_dtb(content);
	}

	/// risc-box patch: attaches a network backend to the virtio-net device.
	/// Without this the guest sees a NIC with no link partner.
	///
	/// # Arguments
	/// * `backend`
	pub fn setup_network(&mut self, backend: Box<dyn crate::net::NetBackend>) {
		self.cpu.get_mut_mmu().get_mut_net().set_backend(backend);
	}

	/// risc-box patch: inject one Linux input event (type, code, value) into the
	/// virtio-input device. A burst of events must be terminated with an
	/// EV_SYN/SYN_REPORT (0,0,0) so the guest input core dispatches the group.
	pub fn push_input_event(&mut self, kind: u16, code: u16, value: u32) {
		self.cpu.get_mut_mmu().get_mut_input().push_event(kind, code, value);
	}

	/// risc-box patch: take up to `max` bytes of audio the guest has played.
	/// Interleaved signed 16-bit little-endian; pair it with `audio_format`.
	pub fn take_audio(&mut self, max: usize) -> Vec<u8> {
		self.cpu.get_mut_mmu().get_mut_snd().take_pcm(max)
	}

	/// risc-box patch: (rate, channels, playing, pending bytes, dropped bytes)
	/// of the virtio-snd stream.
	pub fn audio_state(&mut self) -> (u32, u8, bool, usize, u64) {
		let snd = self.cpu.get_mut_mmu().get_mut_snd();
		let (rate, ch, playing) = snd.format();
		(rate, ch, playing, snd.pending_bytes(), snd.dropped_bytes())
	}

	/// risc-box patch: the max value of the absolute coordinate space the
	/// virtio-input pointer exposes (both axes are 0..=this).
	pub fn input_abs_max() -> i32 {
		crate::device::virtio_input::VirtioInput::abs_max()
	}

	/// Updates XLEN (the width of an integer register in bits) in CPU.
	///
	/// # Arguments
	/// * `xlen`
	pub fn update_xlen(&mut self, xlen: Xlen) {
		self.cpu.update_xlen(xlen);
	}

	/// Enables or disables page cache optimization.
	/// Page cache optimization is experimental feature.
	/// See [`Mmu`](./mmu/struct.Mmu.html) for the detail.
	///
	/// # Arguments
	/// * `enabled`
	pub fn enable_page_cache(&mut self, enabled: bool) {
		self.cpu.get_mut_mmu().enable_page_cache(enabled);
	}

	/// Returns mutable reference to `Terminal`.
	pub fn get_mut_terminal(&mut self) -> &mut Box<dyn Terminal> {
		self.cpu.get_mut_terminal()
	}

	/// Returns immutable reference to `Cpu`.
	/// risc-box patch: bulk read of guest PHYSICAL memory, no side effects —
	/// the framebuffer scanout path (the app reads the simple-framebuffer
	/// region the default DTB reserves at the top of DRAM).
	// risc-box patch (debug aid): stores that landed in the framebuffer window.
	pub fn fb_writes(&self) -> u64 {
		self.cpu.get_mmu().fb_writes()
	}

	// risc-box patch (measurement): framebuffer bytes painted, the numerator of
	// an honest frame rate (see MemoryWrapper::fb_bytes).
	pub fn fb_bytes(&self) -> u64 {
		self.cpu.get_mmu().fb_bytes()
	}

	pub fn read_physical_range(&self, p_address: u64, out: &mut [u8]) {
		self.cpu.get_mmu().read_physical_range(p_address, out);
	}

	/// risc-box patch: fill `out` with the current display contents, whichever
	/// device is driving it.
	///
	/// Two display paths exist on purpose. A guest whose kernel has
	/// CONFIG_DRM_VIRTIO_GPU binds the virtio-gpu at 0x10005000 and pushes its
	/// pixels to us; one without it writes the simple-framebuffer at
	/// `fallback_base` exactly as before. Hiding the choice behind one call
	/// means the scan path has a single source of truth and an older image is
	/// never stranded. Returns true when the pixels came from the GPU.
	pub fn read_display(&self, fallback_base: u64, out: &mut [u8], prefer_gpu: bool) -> bool {
		if prefer_gpu {
			let dims = match self.cpu.get_mmu().get_gpu().scanout() {
				// Only serve the GPU scanout when its geometry matches what
				// the caller sized `out` for. A mode change the host has not
				// caught up with yet must not be blitted through a buffer of
				// the old size.
				Some((w, h, pixels)) if (w as usize) * (h as usize) * 4 == out.len() => {
					out.copy_from_slice(pixels);
					Some((w as usize, h as usize))
				}
				_ => None,
			};
			if let Some((w, h)) = dims {
				// The pointer lives on its own plane, so it is NOT in the
				// scanout the guest just handed us — compose it here or the
				// mouse is invisible.
				self.cpu.get_mmu().get_gpu().compose_cursor(out, w, h);
				return true;
			}
		}
		self.cpu.get_mmu().read_physical_range(fallback_base, out);
		false
	}

	/// risc-box patch: the display controller's geometry, if the guest is
	/// driving it. The mode is the guest's to choose at runtime, so the host
	/// has to ask rather than assume the DTB's numbers.
	/// risc-box patch: OPL register writes the guest has made since the last
	/// call. The host applies these to a native OPL3 chip and mixes the
	/// result into the audio stream — the guest does the MIDI bookkeeping,
	/// the host does the per-sample work.
	pub fn opl_take_writes(&mut self) -> Vec<(u16, u8)> {
		self.cpu.get_mut_mmu().get_mut_opl().drain()
	}

	/// Whether the guest has ever driven the OPL at all, so a host with no
	/// music to mix can skip the synth entirely.
	pub fn opl_active(&mut self) -> bool {
		self.cpu.get_mut_mmu().get_mut_opl().active()
	}

	pub fn gpu_cursor(&self) -> Option<(u32, i64, i64, u64)> {
		self.cpu.get_mmu().get_gpu().cursor_state()
	}

	/// risc-box patch (tier2 feature): coverage-mode region dispatcher.
	#[cfg(feature = "tier2")]
	pub fn tier2_enable(&mut self, dump: Option<&std::path::Path>) {
		self.cpu.tier2_enable(dump);
	}

	#[cfg(feature = "tier2")]
	pub fn tier2_stats(&self) -> (u64, u64, usize, usize) {
		self.cpu.tier2_stats()
	}

	#[cfg(feature = "aot")]
	pub fn aot_enable(&mut self) {
		self.cpu.aot_enable();
	}

	#[cfg(feature = "aot")]
	pub fn aot_baked(&self) -> usize {
		self.cpu.aot_baked()
	}

	pub fn gpu_flushes(&self) -> u64 {
		self.cpu.get_mmu().get_gpu().flushes()
	}

	/// risc-box patch: bytes named by scanout flush rects — the GPU path's
	/// painted-bytes counter (see MemoryWrapper::fb_bytes for the simplefb's).
	pub fn gpu_flush_bytes(&self) -> u64 {
		self.cpu.get_mmu().get_gpu().flush_bytes()
	}

	pub fn gpu_mode(&self) -> Option<(u32, u32)> {
		self.cpu.get_mmu().get_gpu().scanout().map(|(w, h, _)| (w, h))
	}

	/// risc-box patch: the region the guest has flushed since the last call.
	/// This is the payoff over a simple-framebuffer: the guest NAMES what
	/// changed, so the host need not diff a whole frame to find it.
	pub fn gpu_take_dirty(&mut self) -> Option<(u32, u32, u32, u32)> {
		self.cpu.get_mut_mmu().get_mut_gpu().take_dirty()
			.map(|r| (r.x, r.y, r.width, r.height))
	}

	pub fn get_cpu(&self) -> &Cpu {
		&self.cpu
	}

	/// Returns mutable reference to `Cpu`.
	pub fn get_mut_cpu(&mut self) -> &mut Cpu {
		&mut self.cpu
	}

	/// Returns a virtual address corresponding to symbol strings
	///
	/// # Arguments
	/// * `s` Symbol strings
	pub fn get_addredd_of_symbol(&self, s: &String) -> Option<u64> {
		match self.symbol_map.get(s) {
			Some(address) => Some(*address),
			None => None
		}
	}
}

#[cfg(test)]
mod test_emulator {
	use terminal::DummyTerminal;
	use super::*;

	fn create_emu() -> Emulator {
		Emulator::new(
			Box::new(DummyTerminal::new())
		)
	}

	#[test]
	fn initialize() {
		let _emu = create_emu();
	}

	#[test]
	#[ignore]
	fn run() {
	}

	#[test]
	#[ignore]
	fn run_program() {
	}

	#[test]
	#[ignore]
	fn run_test() {
	}

	#[test]
	#[ignore]
	fn tick() {
	}

	#[test]
	#[ignore]
	fn setup_program() {
	}

	#[test]
	#[ignore]
	fn load_program_for_symbols() {
	}

	#[test]
	#[ignore]
	fn setup_filesystem() {
	}

	#[test]
	#[ignore]
	fn setup_dtb() {
	}

	#[test]
	#[ignore]
	fn update_xlen() {
	}

	#[test]
	#[ignore]
	fn enable_page_cache() {
	}

	#[test]
	#[ignore]
	fn get_addredd_of_symbol() {
	}
}
