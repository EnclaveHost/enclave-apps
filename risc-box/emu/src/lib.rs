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
extern crate miniz_oxide; // risc-box patch: snapshot deflate

use self::fnv::FnvHashMap;

pub mod cpu;
pub mod terminal;
pub mod net; // risc-box patch
pub mod default_terminal;
pub mod memory;
pub mod mmu;
pub mod elf_analyzer;
pub mod device;
pub mod snapshot; // risc-box patch: whole-machine snapshot/restore
pub mod disk_image; // risc-box patch: the disk base image, held compressed
#[cfg(feature = "jit")]
pub mod jit; // risc-box patch: PLATFORM-JIT.md translator (feature-gated)
#[cfg(feature = "codegen")]
pub use cpu::jit_selftest; // risc-box patch: the live JIT vs the interpreter

use cpu::{Cpu, Xlen};
use elf_analyzer::{ElfAnalyzer};
use terminal::Terminal;
use snapshot::{De, Ser}; // risc-box patch
pub use memory::RamImage;
use std::sync::Arc;

/// risc-box patch (fork): a snapshot inflated ONCE into shareable form —
/// the RAM as a copy-on-write image, the device sections as bytes, the disk
/// delta as bytes. Any number of machines resume from it; each adoption is
/// O(chunks) reference bumps plus the device state, not an inflate. A
/// running machine can publish itself the same way (`Emulator::image`), so
/// forks need not go through a bucket at all.
pub struct SnapshotImage {
	pub meta: SnapshotMeta,
	sections: Vec<([u8; 4], Vec<u8>)>,
	ram: RamImage,
	ram_pages_kept: u64,
	ram_pages_total: u64,
}

impl SnapshotImage {
	/// Bytes the image itself holds (its RAM chunks + device sections).
	pub fn footprint(&self) -> usize {
		self.ram.footprint() + self.sections.iter().map(|(_, p)| p.len()).sum::<usize>()
	}
}

/// risc-box patch: what a machine costs the host right now, in bytes.
pub struct Footprint {
	pub ram_owned: usize,
	pub ram_shared: usize,
	pub disk_overlay: usize,
}

/// risc-box patch (snapshot): what a snapshot or restore reports back.
pub struct SnapshotInfo {
	pub bytes: usize,
	pub ram_bytes: u64,
	pub ram_pages_kept: u64,
	pub ram_pages_total: u64,
	pub delta_blocks: usize,
	pub taken_unix: u64,
	pub identity: String,
	pub emu_version: String,
}

/// risc-box patch (snapshot): the META section, readable without restoring.
pub struct SnapshotMeta {
	pub identity: String,
	pub ram_bytes: u64,
	pub disk_len: u64,
	pub taken_unix: u64,
	pub emu_version: String,
}

/// The sections a restore must see before it will hand the machine back.
const REQUIRED_SECTIONS: [&[u8; 4]; 14] = [
	b"CPU_", b"MMU_", b"DTB_", b"CLNT", b"PLIC", b"UART", b"VBLK", b"VNET",
	b"VINP", b"VSND", b"VGPU", b"OPL_", b"RAM_", b"DDLT",
];

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

	// risc-box patch (SMP): harts 1.. (hart 0 is `cpu`), sharing its bus; empty on a one-hart machine
	harts: Vec<Cpu>,
	// instructions each hart runs before the next takes the thread (interleaved SMP)
	slice: u64,
	// setup_dtb was called: set_harts leaves the DTB alone (the caller's describes the cpus)
	dtb_given: bool,
	// harts 1.. run on their own threads (start_threads); hart 0 stays with the caller
	threaded: bool,
	ctl: Arc<HartCtl>,
	// run by each hart thread on itself when it starts (its JIT must be enabled on its own thread: compiled
	// code belongs to the execution view that compiled it)
	hart_init: Option<Arc<dyn Fn(&mut Cpu) + Send + Sync>>,

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

/// risc-box patch (SMP): what the hart threads and the embedder's thread share.
#[derive(Default)]
pub struct HartCtl {
	stop: std::sync::atomic::AtomicBool,
	pause: std::sync::atomic::AtomicBool,
	parked: std::sync::atomic::AtomicUsize,
	running: std::sync::atomic::AtomicUsize,
	slice: std::sync::atomic::AtomicU64
}

/// A hart owned by the Emulator's `harts` (which never reallocates once set), run by exactly one thread.
struct HartPtr(*mut Cpu);
unsafe impl Send for HartPtr {}

/// The body of a hart thread: slices of guest time until told to stop; parked while paused; a hart parked in
/// WFI with nothing pending sleeps briefly instead of spinning a host core.
fn hart_loop(cpu: HartPtr, ctl: Arc<HartCtl>, init: Option<Arc<dyn Fn(&mut Cpu) + Send + Sync>>) {
	use std::sync::atomic::Ordering::*;
	let cpu = unsafe { &mut *cpu.0 };
	if let Some(f) = init {
		f(cpu);
	}
	loop {
		if ctl.stop.load(Acquire) {
			break;
		}
		if ctl.pause.load(Acquire) {
			ctl.parked.fetch_add(1, AcqRel);
			while ctl.pause.load(Acquire) && !ctl.stop.load(Acquire) {
				std::thread::sleep(std::time::Duration::from_micros(50));
			}
			ctl.parked.fetch_sub(1, AcqRel);
			continue;
		}
		cpu.drop_reservation_if_stale();
		cpu.run(ctl.slice.load(Relaxed));
		if cpu.is_parked() {
			std::thread::sleep(std::time::Duration::from_micros(100));
		}
	}
	ctl.running.fetch_sub(1, AcqRel);
}

impl Drop for Emulator {
	fn drop(&mut self) {
		if self.threaded {
			self.stop_threads();
		}
	}
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
			harts: Vec::new(),
			slice: 4096,
			dtb_given: false,
			threaded: false,
			ctl: Arc::new(HartCtl::default()),
			hart_init: None,

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
		if self.harts.is_empty() {
			self.cpu.run(n);
			return;
		}
		if self.threaded {
			// the other harts are running on their own threads; outside run_n this thread holds the device
			// lock (so the embedder's own device calls are safe), inside it hart 0 takes it per access
			self.cpu.get_mmu().dev_unlock();
			self.cpu.run(n);
			self.cpu.get_mmu().dev_lock();
			return;
		}
		// Interleaved SMP: each hart runs a slice in turn; `n` is hart 0's share, the device clock's driver.
		// A hart's LR reservation does not survive another hart running (that hart may have stored to the
		// reserved address), so it is dropped at the start of each slice: an SC across a switch fails, which
		// the ISA allows and every LR/SC loop retries.
		let mut left = n;
		while left > 0 {
			let s = left.min(self.slice);
			self.cpu.drop_reservation();
			self.cpu.run(s);
			for h in self.harts.iter_mut() {
				h.drop_reservation();
				h.run(s);
			}
			left -= s;
		}
	}

	/// risc-box patch (SMP): make this a machine of `n` harts. Call before `setup_program` (or `restore`): the
	/// DTB must describe the same number of cpus.
	pub fn set_harts(&mut self, n: usize) {
		let n = n.max(1);
		if n > 1 && !self.dtb_given && !self.cpu.get_mut_mmu().use_smp_dtb(n) {
			eprintln!("[emu] no built-in DTB for {} harts; the guest will see one cpu", n);
		}
		self.cpu.get_mut_mmu().set_harts(n);
		self.cpu.set_smp(n > 1);
		self.harts = (1..n).map(|h| self.cpu.new_hart(h)).collect();
	}

	pub fn harts(&self) -> usize {
		1 + self.harts.len()
	}

	/// risc-box patch (SMP): run harts 1.. on their own threads from now on, each started with `spawn`
	/// (std::thread natively; the SET build's pthreads in the app). Hart 0 keeps running in `run_n` on the
	/// caller's thread. Returns how many threads started; harts whose spawn failed... none may: false from
	/// `spawn` stops the machine from going parallel at all (they all stay interleaved).
	pub fn start_threads(&mut self, spawn: &mut dyn FnMut(Box<dyn FnOnce() + Send + 'static>) -> bool) -> usize {
		if self.threaded || self.harts.is_empty() {
			return 0;
		}
		self.cpu.get_mmu().dev_lock();
		self.ctl.slice.store(self.slice, std::sync::atomic::Ordering::Relaxed);
		let mut started = 0;
		for h in self.harts.iter_mut() {
			let cpu = HartPtr(h as *mut Cpu);
			let ctl = self.ctl.clone();
			let init = self.hart_init.clone();
			ctl.running.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
			if !spawn(Box::new(move || hart_loop(cpu, ctl, init))) {
				self.ctl.running.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
				break;
			}
			started += 1;
		}
		if started < self.harts.len() {
			// the ones that did start: stop them; this machine stays interleaved
			self.stop_threads();
			self.cpu.get_mmu().dev_unlock();
			return 0;
		}
		self.threaded = true;
		started
	}

	/// risc-box patch (SMP): let the hart threads at the devices while this thread sleeps (outside run_n it
	/// otherwise holds the device lock). Pair with acquire_devices before touching the machine again.
	pub fn release_devices(&self) {
		if self.threaded {
			self.cpu.get_mmu().dev_unlock();
		}
	}

	pub fn acquire_devices(&self) {
		if self.threaded {
			self.cpu.get_mmu().dev_lock();
		}
	}

	/// Park every hart thread between slices (and keep it parked) so the caller may touch their state.
	fn pause_harts(&self) {
		use std::sync::atomic::Ordering::*;
		if !self.threaded {
			return;
		}
		self.ctl.pause.store(true, SeqCst);
		// a hart may be waiting for the device lock this thread holds
		self.cpu.get_mmu().dev_unlock();
		while self.ctl.parked.load(Acquire) < self.ctl.running.load(Acquire) {
			std::thread::yield_now();
		}
		self.cpu.get_mmu().dev_lock();
	}

	fn resume_harts(&self) {
		self.ctl.pause.store(false, std::sync::atomic::Ordering::SeqCst);
	}

	fn stop_threads(&mut self) {
		use std::sync::atomic::Ordering::*;
		self.ctl.stop.store(true, SeqCst);
		self.cpu.get_mmu().dev_unlock();
		while self.ctl.running.load(Acquire) > 0 {
			std::thread::yield_now();
		}
		self.cpu.get_mmu().dev_lock();
		self.ctl.stop.store(false, SeqCst);
		self.threaded = false;
	}

	/// risc-box patch (SMP): instructions per hart per turn on the shared thread.
	pub fn set_hart_slice(&mut self, n: u64) {
		self.slice = n.max(64);
	}

	/// risc-box patch (SMP): (pc, privilege, wfi, retired) of each hart, for diagnosis.
	pub fn hart_states(&self) -> Vec<(u64, u8, bool, u64)> {
		std::iter::once(&self.cpu).chain(self.harts.iter()).map(|c| c.hart_state()).collect()
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
		for h in self.harts.iter_mut() {
			h.update_xlen(match header.e_width {
				32 => Xlen::Bit32,
				_ => Xlen::Bit64
			});
			h.update_pc(header.e_entry);
		}
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
		let ok = self.cpu.get_mut_mmu().set_dtb_framebuffer(width, height);
		if ok {
			// keep the overlay rect math on the true stride
			self.cpu.get_mut_mmu().set_fb_stride(width as u64 * 4);
			// ...and make the virtio-gpu advertise the SAME screen. Otherwise
			// the guest gets two different answers for one display: the DTB's
			// simple-framebuffer at the configured size and a GPU still
			// claiming 1024x768, whose fbdev console then drives a scanout
			// bigger than the host sized its display state for.
			self.cpu.get_mut_mmu().get_mut_gpu().set_display_size(width, height);
		}
		ok
	}

	/// risc-box patch: run the guest's clock off the host's monotonic clock
	/// rather than off retired instructions (see `Clint::set_wall_clock`).
	/// Call before the guest boots: the kernel reads the timebase once.
	pub fn set_wall_clock(&mut self, on: bool) {
		self.cpu.get_mut_mmu().get_mut_clint().set_wall_clock(on);
	}

	/// risc-box patch: give this boot its own kernel entropy seed (see
	/// `Mmu::seed_dtb_rng`). Call after any `setup_dtb`, before the guest runs.
	pub fn seed_rng(&mut self, seed: &[u8; 64]) -> bool {
		self.cpu.get_mut_mmu().seed_dtb_rng(seed)
	}

	pub fn setup_dtb(&mut self, content: Vec<u8>) {
		self.dtb_given = true;
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

	/// risc-box patch: input events queued on the host side, not yet in the
	/// guest's buffers (see VirtioInput::pending_len).
	pub fn input_pending(&self) -> usize {
		self.cpu.get_mmu().get_input().pending_len()
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

	/// risc-box patch (host-side game overlay): pixel rect of fb-window
	/// stores since the last call (see MemoryWrapper::fb_take_rect).
	pub fn fb_take_overlay_rect(&self) -> Option<(u32, u32, u32, u32)> {
		self.cpu.get_mmu().fb_take_rect()
	}

	pub fn overlay_state(&self) -> device::overlay::State {
		self.cpu.get_mmu().overlay_state()
	}

	pub fn overlay_frame(&self) -> Option<(u32, u32, u32, u32, &[u8])> {
		self.cpu.get_mmu().overlay_frame()
	}

	pub fn overlay_frame_id(&self) -> Option<u64> {
		self.cpu.get_mmu().overlay_frame_id()
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

	#[cfg(feature = "tier2")]
	pub fn tier2_miss_top(&self, n: usize) -> Vec<(u64, u64)> {
		self.cpu.tier2_miss_top(n)
	}

	#[cfg(feature = "aot")]
	pub fn aot_enable(&mut self) {
		self.cpu.aot_enable();
		for h in self.harts.iter_mut() {
			h.aot_enable();
		}
	}

	#[cfg(feature = "aot")]
	pub fn aot_baked(&self) -> usize {
		self.cpu.aot_baked()
	}

	#[cfg(feature = "aot")]
	pub fn aot_install_stats(&self) -> (u64, u64) {
		self.cpu.aot_install_stats()
	}

	/// risc-box patch (codegen): the live region JIT over the platform's
	/// enclave:codegen verb. False when the verb is absent or its budget is
	/// spent — the machine then interprets exactly as before.
	#[cfg(feature = "codegen")]
	pub fn jit_enable(&mut self, params: cpu::JitParams) -> bool {
		// harts 1.. enable theirs on their own threads (start_threads); interleaved, they interpret
		let p = params.clone();
		self.hart_init = Some(Arc::new(move |c: &mut Cpu| {
			c.jit_enable(p.clone());
		}));
		self.cpu.jit_enable(params)
	}

	#[cfg(feature = "codegen")]
	pub fn jit_diag(&self) -> Option<String> {
		self.cpu.jit_diag()
	}

	#[cfg(feature = "codegen")]
	pub fn jit_stats(&self) -> Option<cpu::JitStats> {
		self.cpu.jit_stats()
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

	/// risc-box patch (snapshot): serialize the whole machine. `identity`
	/// is the caller's name for the images this machine was booted from (the
	/// app hashes the objects it fetched); a restore must present the same
	/// string, which is what binds the disk delta inside to the base image
	/// it was taken against. `level` is the deflate level (1 fast .. 9 small).
	pub fn snapshot(&self, identity: &str, level: u8) -> (Vec<u8>, SnapshotInfo) {
		let taken = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0);
		let ram = self.ram_bytes.unwrap_or(PROGRAM_MEMORY_CAPACITY);
		let disk_len = self.cpu.get_mmu().get_disk().disk_len() as u64;
		let mut s = Ser::with_capacity(32 << 20);
		s.header();
		let at = s.begin_section(b"META");
		s.str(identity);
		s.u64(ram);
		s.u64(disk_len);
		s.u64(taken);
		s.str(env!("CARGO_PKG_VERSION"));
		s.end_section(at);
		self.pause_harts();
		let stats = self.cpu.snapshot_into(&mut s, level);
		for h in self.harts.iter() {
			h.snapshot_hart(&mut s);
		}
		self.resume_harts();
		let bytes = s.buf.len();
		(s.buf, SnapshotInfo {
			bytes,
			ram_bytes: ram,
			ram_pages_kept: stats.ram.pages_kept,
			ram_pages_total: stats.ram.pages_total,
			delta_blocks: stats.delta_blocks,
			taken_unix: taken,
			identity: identity.to_string(),
			emu_version: env!("CARGO_PKG_VERSION").to_string(),
		})
	}

	/// risc-box patch (snapshot): read a snapshot's META without restoring.
	pub fn snapshot_meta(data: &[u8]) -> Result<SnapshotMeta, String> {
		let secs = snapshot::sections(data)?;
		let (tag, payload) = secs.first().ok_or("snapshot: no sections")?;
		if tag != b"META" {
			return Err("snapshot: first section is not META".into());
		}
		let mut r = De::new(payload);
		let identity = r.str()?.to_string();
		let ram_bytes = r.u64()?;
		let disk_len = r.u64()?;
		let taken_unix = r.u64()?;
		let emu_version = r.str()?.to_string();
		r.finish("META")?;
		Ok(SnapshotMeta { identity, ram_bytes, disk_len, taken_unix, emu_version })
	}

	/// risc-box patch (snapshot): resume a machine from `snapshot()` output.
	///
	/// Call on a fresh emulator prepared exactly as for a cold boot up to but
	/// NOT including `setup_program` — RAM size, framebuffer, clock source
	/// and, crucially, `setup_filesystem` with the same base image — then
	/// this instead of `setup_program`. The kernel image is not needed: the
	/// guest's memory already holds it. Refuses (leaving the caller to cold
	/// boot) when the identity, RAM size or base disk differ from the
	/// snapshot's, or when the format has drifted.
	pub fn restore(&mut self, data: &[u8], identity: &str) -> Result<SnapshotInfo, String> {
		let image = Self::load_image(data)?;
		self.restore_image(&image, identity)
	}

	/// risc-box patch (fork): inflate a snapshot into a shareable image.
	pub fn load_image(data: &[u8]) -> Result<SnapshotImage, String> {
		let meta = Self::snapshot_meta(data)?;
		let secs = snapshot::sections(data)?;
		let mut sections = Vec::new();
		let mut ram: Option<(RamImage, snapshot::RamStats)> = None;
		for (tag, payload) in secs.iter().skip(1) {
			match tag {
				b"RAM_" => ram = Some(snapshot::decode_ram_image(payload)?),
				_ => sections.push((*tag, payload.to_vec())),
			}
		}
		let (ram, st) = ram.ok_or("snapshot: missing section \"RAM_\"")?;
		if ram.len() as u64 != meta.ram_bytes {
			return Err("snapshot: RAM section disagrees with META about the RAM size".into());
		}
		Ok(SnapshotImage {
			meta,
			sections,
			ram,
			ram_pages_kept: st.pages_kept,
			ram_pages_total: st.pages_total,
		})
	}

	/// risc-box patch (fork): resume from an image. Same preconditions as
	/// `restore`; the RAM is adopted copy-on-write rather than copied.
	pub fn restore_image(&mut self, image: &SnapshotImage, identity: &str) -> Result<SnapshotInfo, String> {
		self.pause_harts();
		let r = self.restore_image_paused(image, identity);
		self.resume_harts();
		r
	}

	fn restore_image_paused(&mut self, image: &SnapshotImage, identity: &str) -> Result<SnapshotInfo, String> {
		let meta = &image.meta;
		if meta.identity != identity {
			return Err(format!(
				"snapshot was taken against different images (its identity {:?}, this machine's {:?})",
				meta.identity, identity
			));
		}
		let ram = self.ram_bytes.unwrap_or(PROGRAM_MEMORY_CAPACITY);
		if meta.ram_bytes != ram {
			return Err(format!(
				"snapshot has {} MiB of guest RAM but this machine is configured for {} MiB",
				meta.ram_bytes >> 20, ram >> 20
			));
		}
		let disk_len = self.cpu.get_mmu().get_disk().disk_len() as u64;
		if meta.disk_len != disk_len {
			return Err(format!(
				"snapshot expects a {}-byte base disk but {} bytes are loaded",
				meta.disk_len, disk_len
			));
		}
		self.is_test = false;
		self.tohost_addr = 0;
		self.cpu.update_xlen(Xlen::Bit64);
		self.cpu.get_mut_mmu().init_memory(ram);
		self.cpu.get_mut_mmu().adopt_ram(&image.ram);
		let mut seen: Vec<[u8; 4]> = vec![*b"RAM_"];
		let mut stats = snapshot::RestoreStats::default();
		let mut harts_seen = 0;
		for (tag, payload) in image.sections.iter() {
			if tag[..3] == *b"HRT" {
				let h = (tag[3].wrapping_sub(b'0')) as usize;
				match self.harts.get_mut(h.wrapping_sub(1)) {
					Some(c) => c.restore_hart(payload)?,
					None => return Err(format!("snapshot has hart {} but this machine has {} hart(s)", h, 1 + self.harts.len()))
				}
				harts_seen += 1;
				seen.push(*tag);
				continue;
			}
			if !self.cpu.restore_section(tag, payload, &mut stats)? {
				return Err(format!(
					"snapshot: unknown section {:?} (written by a newer emulator?)",
					String::from_utf8_lossy(tag)
				));
			}
			seen.push(*tag);
		}
		if harts_seen != self.harts.len() {
			return Err(format!("snapshot has {} hart(s) but this machine has {}", 1 + harts_seen, 1 + self.harts.len()));
		}
		for req in REQUIRED_SECTIONS.iter() {
			if !seen.contains(req) {
				return Err(format!("snapshot: missing section {:?}", String::from_utf8_lossy(*req)));
			}
		}
		Ok(SnapshotInfo {
			bytes: image.footprint(),
			ram_bytes: ram,
			ram_pages_kept: image.ram_pages_kept,
			ram_pages_total: image.ram_pages_total,
			delta_blocks: stats.delta_blocks,
			taken_unix: meta.taken_unix,
			identity: meta.identity.clone(),
			emu_version: meta.emu_version.clone(),
		})
	}

	/// risc-box patch (fork): publish the RUNNING machine as an image others
	/// can resume from, without a bucket round trip. Its RAM becomes shared
	/// (this machine copies a chunk on its next write to it); the device
	/// state is serialized as for a snapshot. Milliseconds, not seconds.
	pub fn image(&mut self, identity: &str) -> SnapshotImage {
		let taken = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0);
		let ram_bytes = self.ram_bytes.unwrap_or(PROGRAM_MEMORY_CAPACITY);
		let disk_len = self.cpu.get_mmu().get_disk().disk_len() as u64;
		// device sections only: level is irrelevant without RAM, and the
		// delta is small
		let mut s = Ser::new();
		self.pause_harts();
		self.cpu.snapshot_into_no_ram(&mut s, 1);
		for h in self.harts.iter() {
			h.snapshot_hart(&mut s);
		}
		let secs = snapshot::sections_unframed(&s.buf);
		let ram = self.cpu.get_mut_mmu().share_ram();
		self.resume_harts();
		let pages_total = (ram.len() + snapshot::PAGE - 1) / snapshot::PAGE;
		let pages_kept = (0..pages_total).filter(|&i| ram.page(i).is_some()).count();
		SnapshotImage {
			meta: SnapshotMeta {
				identity: identity.to_string(),
				ram_bytes,
				disk_len,
				taken_unix: taken,
				emu_version: env!("CARGO_PKG_VERSION").to_string(),
			},
			sections: secs,
			ram,
			ram_pages_kept: pages_kept as u64,
			ram_pages_total: pages_total as u64,
		}
	}

	/// risc-box patch: like `setup_filesystem`, with an image other machines
	/// share. Nothing is copied.
	pub fn setup_filesystem_shared(&mut self, base: Arc<disk_image::DiskImage>) {
		self.cpu.get_mut_mmu().init_disk_shared(base);
	}

	/// risc-box patch: the base disk this machine boots from, for sharing.
	pub fn disk_base(&self) -> Arc<disk_image::DiskImage> {
		self.cpu.get_mmu().get_disk().base()
	}

	/// risc-box patch: what this machine costs the host right now.
	pub fn footprint(&self) -> Footprint {
		let (ram_owned, ram_shared, disk_overlay) = self.cpu.get_mmu().footprint();
		Footprint { ram_owned, ram_shared, disk_overlay }
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
