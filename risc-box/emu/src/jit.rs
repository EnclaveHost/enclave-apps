//! risc-box patch (jit feature): the app-side translator half of
//! PLATFORM-JIT.md — real superblock ops (cpu::BlockOp) emitted as a wasm
//! module, keeping the interpreter-bail contract exec_block already keeps:
//!
//! - every exit leaves pc EXACT: a bail (unsupported op, TLB miss, a store
//!   the interpreter must perform itself) sets pc to the op that did not run
//!   and returns how many ops did; a taken branch/jump sets its target;
//!   falling off the end sets the fallthrough. Exits return compile-time
//!   constants, so the body carries no per-op retired counter or pc store.
//! - loads/stores reach guest RAM the way the interpreter's fast path does:
//!   the access must stay inside one 4 KiB page, translate through the
//!   software TLB (hit only: miss or stale meta bails, the interpreter walks
//!   and fills so the retry hits), and land inside DRAM. RAM is either a
//!   flat window (tests, the prototype) or risc-box's chunked copy-on-write
//!   RAM (memory.rs): reads go through the chunk read-pointer table, stores
//!   through the write-pointer table and bail when it is null (a Zero or
//!   Shared chunk — the interpreter materializes it), when the target page
//!   is marked executable (the interpreter's store bumps the write-snoop
//!   generation), or when it lands in a window with store-side bookkeeping
//!   (the framebuffer's overlay rectangle). Generated code therefore never
//!   changes the code generation, and never writes memory the interpreter
//!   would have written differently.
//! - nothing about the machine's ADDRESS is baked in. The code reads a
//!   small context block at entry (state base, chunk tables, a pc bias), so
//!   one compiled module serves a Cpu wherever it lives in linear memory and
//!   the same code mapped at another page-aligned address (ASLR, another
//!   process, another machine): every guest pc in the module is relative,
//!   and runtime pc = module pc + bias.
//! - RV64 only: the caller never forms regions for a Bit32 guest.
//!
//! The equivalence tests live in cpu.rs (same module as the private state
//! they compare) and run each generated module under wasmtime — a
//! dev-dependency; nothing here links wasmtime into the shipped app.

use cpu::BlockOp;
use cpu::*;
use std::collections::HashMap;

/// The context block: u64 cells at `Layout::ctx` the generated code reads
/// at entry. In production these are refreshed by the dispatcher before a
/// call (base/tables once per run(), bias per call).
pub const CTX_BASE: u64 = 0; // address every state offset is relative to
pub const CTX_RD: u64 = 8; // chunked RAM: read pointer per 64 KiB chunk
pub const CTX_WR: u64 = 16; // chunked RAM: write pointer per chunk (null = not owned)
pub const CTX_MARKS: u64 = 24; // chunked RAM: one byte per 4 KiB page, nonzero = executable
pub const CTX_BIAS: u64 = 32; // runtime guest pc = module pc + bias
pub const CTX_CELLS: usize = 5;

const CHUNK_SHIFT: u64 = ::memory::CHUNK_SHIFT as u64;
const CHUNK_MASK: u64 = (1u64 << CHUNK_SHIFT) - 1;

/// Where the machine's state lives inside the imported linear memory, and
/// what that memory is. State offsets (x/f/pc/gen and the TLB) are relative
/// to the state base read from the context block; tests point the base at
/// 0 and use absolute addresses.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Layout {
	/// The imported memory: i64 addresses (wasm64) or i32.
	pub memory64: bool,
	/// The imported memory is shared (SET builds); requires max_pages.
	pub shared: bool,
	/// Declared maximum of the import, in 64 KiB pages. An import matches a
	/// memory whose maximum is at most this.
	pub max_pages: Option<u64>,
	/// Absolute address of the context block.
	pub ctx: u64,
	pub x_base: u64, // x[32] as i64
	pub f_base: u64, // f[32] as raw 8-byte cells
	pub pc_addr: u64, // u64
	pub gen_addr: u64, // u32 write-snoop generation cell (flat RAM only)
	pub baked_gen: u32, // generation a flat-RAM module was built against
	/// u64: the fcsr CSR (fflags bits 0-4, frm bits 5-7). FDIV's divide-by-
	/// zero flag and the float-CSR ops reach it here.
	pub fcsr_addr: u64,
	/// LR/SC: the reservation flag (one byte, 0/1) and the virtual address
	/// it holds (u64) - Cpu::is_reservation_set / Cpu::reservation.
	pub res_flag_addr: u64,
	pub res_addr_addr: u64,
	/// Some(_) when the guest runs under paging: memory ops probe the
	/// emulator's software TLB (hit -> translated physical; miss/meta-stale
	/// -> bail). None for bare/physical addressing.
	pub tlb: Option<TlbLayout>,
	pub guest_dram_base: u64, // 0x8000_0000
	pub dram_len: u64,
	pub ram: Ram,
}

/// How guest DRAM is laid out in linear memory.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ram {
	/// One flat window: linear = dram_base + (phys - guest_dram_base). Every
	/// store re-checks the write-snoop generation (gen_addr) afterwards.
	Flat { dram_base: u64 },
	/// memory.rs: 64 KiB chunks reached through the context block's
	/// read/write pointer tables, with the executable-page marks beside them.
	/// `store_bail` lists DRAM-offset windows [lo, hi) whose stores carry
	/// interpreter-side bookkeeping and so always bail.
	Chunked { store_bail: Vec<(u64, u64)> },
}

/// Where the software TLB's READ and WRITE ways live (offsets from the state
/// base), mirroring Mmu::translate_address's hit path: set = (vaddr >> 12)
/// & (sets-1); hit iff tags[set] == (vaddr & !0xfff) | 1 && metas[set] ==
/// *meta_cache.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TlbLayout {
	pub sets: u32, // power of two (the emulator uses 512)
	pub read_tags: u64,
	pub read_metas: u64,
	pub read_ppns: u64,
	pub write_tags: u64,
	pub write_metas: u64,
	pub write_ppns: u64,
	pub meta_cache: u64, // u32 cell holding tlb_meta_cache
}

fn uleb(out: &mut Vec<u8>, mut v: u64) {
	loop {
		let b = (v & 0x7f) as u8;
		v >>= 7;
		if v == 0 {
			out.push(b);
			break;
		}
		out.push(b | 0x80);
	}
}

fn sleb(out: &mut Vec<u8>, mut v: i64) {
	loop {
		let b = (v & 0x7f) as u8;
		v >>= 7;
		let sign = b & 0x40 != 0;
		if (v == 0 && !sign) || (v == -1 && sign) {
			out.push(b);
			break;
		}
		out.push(b | 0x80);
	}
}

// opcode bytes used below
const UNREACHABLE: u8 = 0x00;
const BLOCK: u8 = 0x02;
const LOOP: u8 = 0x03;
const IF: u8 = 0x04;
const ELSE: u8 = 0x05;
const END: u8 = 0x0b;
const BR: u8 = 0x0c;
const BR_TABLE: u8 = 0x0e;
const RETURN: u8 = 0x0f;
const VOID: u8 = 0x40;
const LOCAL_GET: u8 = 0x20;
const LOCAL_SET: u8 = 0x21;
const LOCAL_TEE: u8 = 0x22;
const I32_LOAD: u8 = 0x28;
const I64_LOAD: u8 = 0x29;
const I32_LOAD8_U: u8 = 0x2d;
const I64_STORE: u8 = 0x37;
const I32_CONST: u8 = 0x41;
const I64_CONST: u8 = 0x42;
const I32_GE_U: u8 = 0x4f;
const I32_NE: u8 = 0x47;
const I64_EQZ: u8 = 0x50;
const I64_EQ: u8 = 0x51;
const I64_NE: u8 = 0x52;
const I64_LT_U: u8 = 0x54;
const I64_GT_U: u8 = 0x56;
const I64_GE_U: u8 = 0x5a;
const I32_ADD: u8 = 0x6a;
const I64_ADD: u8 = 0x7c;
const I64_SUB: u8 = 0x7d;
const I64_AND: u8 = 0x83;
const I64_OR: u8 = 0x84;
const I64_SHL: u8 = 0x86;
const I64_SHR_U: u8 = 0x88;
const I32_WRAP_I64: u8 = 0xa7;
const I64_EXTEND_I32_U: u8 = 0xad;

/// Local indices. Every generated function has the region shape
/// (fuel: i64, entry: i32) -> i64. Address-typed locals (base, the chunk
/// tables, addr) are i64 under memory64, i32 otherwise. The guest
/// registers a region touches live in locals from FIRST_REG_LOCAL on: loaded
/// once at entry, written back (the ones it wrote) at the single exit.
#[derive(Clone, Copy)]
struct Locals {
	fuel: u32,
	entry: u32,
	scratch: u32,
	cur: u32,
	retired: u32,
	scratch2: u32,
	bias: u32,
	tpc: u32,
	pcv: u32,
	base: u32,
	rdt: u32,
	wrt: u32,
	marks: u32,
	addr: u32,
	meta: u32,
	// i64 temporaries for the ops that need values twice (MULH's 128-bit
	// product, an AMO's old value, LR/SC's address)
	t0: u32,
	t1: u32,
	t2: u32,
}

// params fuel, entry | i64 scratch | i32 cur | i64 retired, scratch2, bias,
// tpc, pcv | addr base, rdt, wrt, marks, addr | i32 meta | i64 t0, t1, t2 |
// i64 registers
const LOCALS: Locals = Locals {
	fuel: 0, entry: 1, scratch: 2, cur: 3, retired: 4, scratch2: 5, bias: 6, tpc: 7, pcv: 8,
	base: 9, rdt: 10, wrt: 11, marks: 12, addr: 13, meta: 14, t0: 15, t1: 16, t2: 17,
};
const FIRST_REG_LOCAL: u32 = 18;
const NONE: u32 = u32::MAX;

struct Emit<'a> {
	code: Vec<u8>,
	lay: &'a Layout,
	l: Locals,
	// how many labels opened inside the current block's code enclose the
	// emission point — a br to the dispatch loop (or the exit, one further
	// out) must add this to its depth
	if_depth: u32,
	// guest block start pc (module-relative) -> block index
	targets: HashMap<u64, u32>,
	// br depth from the current block's code to the dispatch loop head
	loop_depth: u32,
	// label index of the indirect-jump dispatcher, when the region has one
	dispatch: Option<u32>,
	// the local caching x[r] / the bits of f[r] (NONE: not cached)
	xl: [u32; 32],
	fl: [u32; 32],
	// registers written somewhere in the region (written back at exit)
	xdirty: u32,
	fdirty: u32,
	reg_locals: u32,
	// emit_block: an untranslatable first op aborts emission
	strict: bool,
}

impl<'a> Emit<'a> {
	fn new(lay: &'a Layout) -> Emit<'a> {
		Emit {
			code: Vec::new(), lay, l: LOCALS, if_depth: 0, targets: HashMap::new(),
			loop_depth: 0, dispatch: None, xl: [NONE; 32], fl: [NONE; 32], xdirty: 0,
			fdirty: 0, reg_locals: 0, strict: false,
		}
	}
	fn op(&mut self, b: u8) {
		self.code.push(b);
	}
	fn i32c(&mut self, v: i32) {
		self.code.push(I32_CONST);
		sleb(&mut self.code, v as i64);
	}
	fn i64c(&mut self, v: i64) {
		self.code.push(I64_CONST);
		sleb(&mut self.code, v);
	}
	fn idx(&mut self, v: u64) {
		uleb(&mut self.code, v);
	}
	fn lget(&mut self, i: u32) {
		self.op(LOCAL_GET);
		self.idx(i as u64);
	}
	fn lset(&mut self, i: u32) {
		self.op(LOCAL_SET);
		self.idx(i as u64);
	}
	fn ltee(&mut self, i: u32) {
		self.op(LOCAL_TEE);
		self.idx(i as u64);
	}
	fn memarg(&mut self, align: u8, off: u64) {
		self.code.push(align);
		uleb(&mut self.code, off);
	}
	fn m64(&self) -> bool {
		self.lay.memory64
	}
	/// i64 on the stack -> an address of the memory's index type
	fn to_addr(&mut self) {
		if !self.m64() {
			self.op(I32_WRAP_I64);
		}
	}
	fn addr_add(&mut self) {
		let o = if self.m64() { I64_ADD } else { I32_ADD };
		self.op(o);
	}
	fn addr_const(&mut self, v: u64) {
		if self.m64() {
			self.i64c(v as i64)
		} else {
			self.i32c(v as u32 as i32)
		}
	}
	/// stack: address -> the pointer-width value stored there (index type)
	fn load_ptr(&mut self) {
		if self.m64() {
			self.op(I64_LOAD);
			self.memarg(3, 0);
		} else {
			self.op(I32_LOAD);
			self.memarg(2, 0);
		}
	}
	fn chunked(&self) -> bool {
		matches!(self.lay.ram, Ram::Chunked { .. })
	}

	/// Give every register the region's ops can touch a local.
	fn plan_registers(&mut self, blocks: &[(u64, Vec<BlockOp>)]) {
		let (mut xm, mut fm) = (0u32, 0u32);
		for &(_, ref ops) in blocks {
			for op in ops.iter().filter(|o| translatable(o)) {
				let m = 1u32 << op.rd | 1u32 << op.rs1 | 1u32 << op.rs2;
				xm |= m;
				if matches!(op.kind, HOT_FLD | HOT_FLW | HOT_FSD | HOT_FSW | HOT_FADD_D | HOT_FSUB_D
					| HOT_FMUL_D | HOT_FDIV_D | HOT_FSGNJ_D | HOT_FMV_X_D | HOT_FMV_D_X | HOT_FCVT_D_W)
				{
					fm |= m;
				}
				// the float table ops: an over-approximation (FEQ's rd is an x
				// register) costs one unused local, never a wrong value
				if matches!(table_op(op), Some(TableOp::Fs(_)) | Some(TableOp::Fd(_))
					| Some(TableOp::CvtDS) | Some(TableOp::CvtSD) | Some(TableOp::MvXW)
					| Some(TableOp::MvWX))
				{
					fm |= m | 1u32 << ((op.word >> 27) & 0x1f);
				}
			}
		}
		let mut next = FIRST_REG_LOCAL;
		for r in 1..32 {
			if xm & (1 << r) != 0 {
				self.xl[r] = next;
				next += 1;
			}
		}
		for r in 0..32 {
			if fm & (1 << r) != 0 {
				self.fl[r] = next;
				next += 1;
			}
		}
		self.reg_locals = next - FIRST_REG_LOCAL;
	}

	/// Read the context block (and the cached registers) into locals.
	fn prologue(&mut self) {
		let ctx = self.lay.ctx;
		let (base, bias) = (self.l.base, self.l.bias);
		self.addr_const(ctx);
		self.op(I64_LOAD);
		self.memarg(3, CTX_BASE);
		self.to_addr();
		self.lset(base);
		self.addr_const(ctx);
		self.op(I64_LOAD);
		self.memarg(3, CTX_BIAS);
		self.lset(bias);
		if self.chunked() {
			for (cell, local) in [(CTX_RD, self.l.rdt), (CTX_WR, self.l.wrt), (CTX_MARKS, self.l.marks)] {
				self.addr_const(ctx);
				self.op(I64_LOAD);
				self.memarg(3, cell);
				self.to_addr();
				self.lset(local);
			}
		}
		if let Some(t) = self.lay.tlb.clone() {
			// the interpreter alone changes the meta; it cannot move mid-call
			self.lget(base);
			self.op(I32_LOAD);
			self.memarg(2, t.meta_cache);
			self.lset(self.l.meta);
		}
		for r in 1..32 {
			if self.xl[r] != NONE {
				self.lget(base);
				self.op(I64_LOAD);
				self.memarg(3, self.lay.x_base + r as u64 * 8);
				self.lset(self.xl[r]);
			}
		}
		for r in 0..32 {
			if self.fl[r] != NONE {
				self.lget(base);
				self.op(I64_LOAD);
				self.memarg(3, self.lay.f_base + r as u64 * 8);
				self.lset(self.fl[r]);
			}
		}
	}

	/// The single exit (after the $exit block): write back the registers
	/// the region wrote and pc, return retired.
	fn epilogue(&mut self) {
		let base = self.l.base;
		for r in 1..32 {
			if self.xdirty & (1 << r) != 0 {
				self.lget(base);
				self.lget(self.xl[r]);
				self.op(I64_STORE);
				self.memarg(3, self.lay.x_base + r as u64 * 8);
			}
		}
		for r in 0..32 {
			if self.fdirty & (1 << r) != 0 {
				self.lget(base);
				self.lget(self.fl[r]);
				self.op(I64_STORE);
				self.memarg(3, self.lay.f_base + r as u64 * 8);
			}
		}
		self.lget(base);
		self.lget(self.l.pcv);
		self.op(I64_STORE);
		self.memarg(3, self.lay.pc_addr);
		self.lget(self.l.retired);
	}

	/// push x[r]
	fn get_x(&mut self, r: u8) {
		if r == 0 {
			self.i64c(0);
			return;
		}
		match self.xl[r as usize] {
			NONE => {
				self.lget(self.l.base);
				self.op(I64_LOAD);
				self.memarg(3, self.lay.x_base + r as u64 * 8);
			}
			l => self.lget(l),
		}
	}
	/// x[rd] <- value: set_x_pre before the value, set_x_post after (a write
	/// to x0 is dropped)
	fn set_x_pre(&mut self, r: u8) {
		if r != 0 && self.xl[r as usize] == NONE {
			self.lget(self.l.base);
		}
	}
	fn set_x_post(&mut self, r: u8) {
		if r == 0 {
			self.op(0x1a); // drop
			return;
		}
		match self.xl[r as usize] {
			NONE => {
				self.op(I64_STORE);
				self.memarg(3, self.lay.x_base + r as u64 * 8);
			}
			l => {
				self.lset(l);
				self.xdirty |= 1 << r;
			}
		}
	}
	/// push f[r] bit pattern as i64
	fn get_f_bits(&mut self, r: u8) {
		match self.fl[r as usize] {
			NONE => {
				self.lget(self.l.base);
				self.op(I64_LOAD);
				self.memarg(3, self.lay.f_base + r as u64 * 8);
			}
			l => self.lget(l),
		}
	}
	fn set_f_pre(&mut self, r: u8) {
		if self.fl[r as usize] == NONE {
			self.lget(self.l.base);
		}
	}
	fn set_f_bits_post(&mut self, r: u8) {
		match self.fl[r as usize] {
			NONE => {
				self.op(I64_STORE);
				self.memarg(3, self.lay.f_base + r as u64 * 8);
			}
			l => {
				self.lset(l);
				self.fdirty |= 1 << r;
			}
		}
	}
	/// push f[r] as f64
	fn get_f(&mut self, r: u8) {
		self.get_f_bits(r);
		self.op(0xbf); // f64.reinterpret_i64 (bit-exact)
	}
	fn set_f_post(&mut self, r: u8) {
		self.op(0xbd); // i64.reinterpret_f64 (bit-exact)
		self.set_f_bits_post(r);
	}
	/// push f[r] as the single it holds: the interpreter keeps a single in
	/// the LOW 32 bits of the register (f32::from_bits(bits as u32))
	fn get_f32(&mut self, r: u8) {
		self.get_f_bits(r);
		self.op(I32_WRAP_I64);
		self.op(0xbe); // f32.reinterpret_i32
	}
	/// f32 on the stack -> f[r] = its bits, ZERO-extended (the interpreter's
	/// f64::from_bits(x.to_bits() as u64))
	fn set_f32_post(&mut self, r: u8) {
		self.op(0xbc); // i32.reinterpret_f32
		self.op(I64_EXTEND_I32_U);
		self.set_f_bits_post(r);
	}
	/// fcsr |= bits (Cpu::set_fcsr_dz and friends)
	fn fcsr_or(&mut self, bits: i64) {
		let (base, a) = (self.l.base, self.lay.fcsr_addr);
		self.lget(base);
		self.lget(base);
		self.op(I64_LOAD);
		self.memarg(3, a);
		self.i64c(bits);
		self.op(I64_OR);
		self.op(I64_STORE);
		self.memarg(3, a);
	}
	/// a 0xfc-prefixed opcode (the saturating float->int conversions)
	fn fc(&mut self, sub: u64) {
		self.op(0xfc);
		uleb(&mut self.code, sub);
	}

	/// push the runtime pc for module pc `rel`
	fn push_pc(&mut self, rel: u64) {
		self.i64c(rel as i64);
		self.lget(self.l.bias);
		self.op(I64_ADD);
	}

	fn add_retired(&mut self, n: u64) {
		if n != 0 {
			let r = self.l.retired;
			self.lget(r);
			self.i64c(n as i64);
			self.op(I64_ADD);
			self.lset(r);
		}
	}

	/// pc <- runtime pc for `rel`, then branch to the single exit.
	fn to_exit(&mut self, rel: u64) {
		self.push_pc(rel);
		self.lset(self.l.pcv);
		self.op(BR);
		let d = self.loop_depth + self.if_depth + 1;
		self.idx(d as u64);
	}

	/// pc <- `pc`, `retired` more retired: if pc names a region block, branch
	/// back to the dispatch loop (which checks fuel first); else leave.
	fn exit(&mut self, pc: u64, retired: u64) {
		self.add_retired(retired);
		match self.targets.get(&pc).copied() {
			Some(idx) => {
				self.i32c(idx as i32);
				self.lset(self.l.cur);
				self.op(BR);
				let d = self.loop_depth + self.if_depth;
				self.idx(d as u64);
			}
			None => self.to_exit(pc),
		}
	}

	/// A bail: pc <- `pc`, leave. Bails NEVER transfer within a region — a
	/// bail pc that happens to be a block start must still hand control
	/// back (a memory bail at a block's first op would otherwise loop
	/// forever re-entering it).
	fn bail(&mut self, pc: u64, retired: u64) {
		self.add_retired(retired);
		self.to_exit(pc);
	}

	/// consume an i32 condition: if nonzero, bail
	fn bail_if(&mut self, pc: u64, retired: u64) {
		self.op(IF);
		self.op(VOID);
		self.if_depth += 1;
		self.bail(pc, retired);
		self.if_depth -= 1;
		self.op(END);
	}

	/// stack: guest VIRTUAL address (i64). Leaves the LINEAR address of the
	/// access (index type) on the stack, or bails with pc at `op_addr`.
	/// Every check branches to ONE bail per access (block $fail), so a
	/// memory op carries a single exit sequence, not one per check.
	fn dram_addr(&mut self, width: u64, op_addr: u64, retired: u64, write: bool) {
		let (sc, sc2, addr) = (self.l.scratch, self.l.scratch2, self.l.addr);
		self.lset(sc);
		self.op(BLOCK); // $done
		self.op(VOID);
		self.op(BLOCK); // $fail
		self.op(VOID);
		// The interpreter's fast path: the whole access inside one 4 KiB
		// page. Anything else (cross-page) takes its byte-wise path there.
		self.lget(sc);
		self.i64c(0xfff);
		self.op(I64_AND);
		self.i64c((0x1000 - width) as i64);
		self.op(I64_GT_U);
		self.br_if(0);
		self.lget(sc);
		if self.lay.tlb.is_some() {
			self.tlb_translate(write);
		}
		// off = phys - guest_dram_base; bail unless [off, off+width) is DRAM
		self.i64c(self.lay.guest_dram_base as i64);
		self.op(I64_SUB);
		self.ltee(sc);
		self.i64c(self.lay.dram_len.wrapping_sub(width) as i64);
		self.op(I64_GT_U);
		self.br_if(0);
		match self.lay.ram.clone() {
			Ram::Flat { dram_base } => {
				self.lget(sc);
				self.to_addr();
				self.addr_const(dram_base);
				self.addr_add();
			}
			Ram::Chunked { store_bail } => {
				if write {
					// an executable page: the interpreter's store bumps the
					// write-snoop generation (and so stops stale code)
					self.lget(self.l.marks);
					self.lget(sc);
					self.i64c(12);
					self.op(I64_SHR_U);
					self.to_addr();
					self.addr_add();
					self.op(I32_LOAD8_U);
					self.memarg(0, 0);
					self.br_if(0);
					for (lo, hi) in store_bail {
						self.lget(sc);
						self.i64c(lo as i64);
						self.op(I64_SUB);
						self.i64c(hi.wrapping_sub(lo) as i64);
						self.op(I64_LT_U);
						self.br_if(0);
					}
				}
				// the chunk's pointer from the read or write table
				let table = if write { self.l.wrt } else { self.l.rdt };
				self.lget(table);
				self.lget(sc);
				self.i64c(CHUNK_SHIFT as i64);
				self.op(I64_SHR_U);
				self.i64c(if self.m64() { 3 } else { 2 });
				self.op(I64_SHL);
				self.to_addr();
				self.addr_add();
				self.load_ptr();
				if write {
					// null: a Zero or Shared chunk. The interpreter's store
					// materializes (copy-on-write) it; the retry runs here.
					if !self.m64() {
						self.op(I64_EXTEND_I32_U);
					}
					self.ltee(sc2);
					self.op(I64_EQZ);
					self.br_if(0);
					self.lget(sc2);
					self.to_addr();
				}
				self.lget(sc);
				self.i64c(CHUNK_MASK as i64);
				self.op(I64_AND);
				self.to_addr();
				self.addr_add();
			}
		}
		self.lset(addr);
		self.op(BR);
		self.idx(1); // $done
		self.op(END); // $fail
		self.if_depth += 1; // inside $done
		self.bail(op_addr, retired);
		self.if_depth -= 1;
		self.op(END); // $done
		self.lget(addr);
	}

	fn br_if(&mut self, depth: u32) {
		self.op(0x0d);
		self.idx(depth as u64);
	}

	/// Inside dram_addr's $fail block. stack: guest virtual address ->
	/// stack: guest PHYSICAL address; a TLB miss or stale meta branches to
	/// the bail. scratch: vaddr; scratch2: set*8.
	fn tlb_translate(&mut self, write: bool) {
		let t = self.lay.tlb.clone().unwrap();
		let (tags, metas, ppns) = match write {
			false => (t.read_tags, t.read_metas, t.read_ppns),
			true => (t.write_tags, t.write_metas, t.write_ppns),
		};
		let (sc, sc2, base) = (self.l.scratch, self.l.scratch2, self.l.base);
		self.lset(sc);
		// scratch2 = ((vaddr >> 12) & (sets-1)) * 8   (tag/ppn entry offset)
		self.lget(sc);
		self.i64c(12);
		self.op(I64_SHR_U);
		self.i64c((t.sets - 1) as i64);
		self.op(I64_AND);
		self.i64c(3);
		self.op(I64_SHL);
		self.lset(sc2);
		// tag hit? tags[set] == (vaddr & !0xfff) | 1
		self.lget(base);
		self.lget(sc2);
		self.to_addr();
		self.addr_add();
		self.op(I64_LOAD);
		self.memarg(3, tags);
		self.lget(sc);
		self.i64c(!0xfffi64);
		self.op(I64_AND);
		self.i64c(1);
		self.op(I64_OR);
		self.op(I64_NE);
		self.br_if(0);
		// meta fresh? metas are u32 per set: offset = scratch2 / 2
		self.lget(base);
		self.lget(sc2);
		self.i64c(1);
		self.op(I64_SHR_U);
		self.to_addr();
		self.addr_add();
		self.op(I32_LOAD);
		self.memarg(2, metas);
		self.lget(self.l.meta);
		self.op(I32_NE);
		self.br_if(0);
		// phys = ppns[set] | (vaddr & 0xfff)
		self.lget(base);
		self.lget(sc2);
		self.to_addr();
		self.addr_add();
		self.op(I64_LOAD);
		self.memarg(3, ppns);
		self.lget(sc);
		self.i64c(0xfff);
		self.op(I64_AND);
		self.op(I64_OR);
	}

	/// Flat RAM only: after a store, if the generation cell moved, exit with
	/// pc = next and the store counted. Chunked stores never reach a marked
	/// page (they bail first), so they cannot move the generation.
	fn gen_check(&mut self, next_pc: u64, retired: u64) {
		if self.chunked() {
			return;
		}
		self.lget(self.l.base);
		self.op(I32_LOAD);
		self.memarg(2, self.lay.gen_addr);
		self.i32c(self.lay.baked_gen as i32);
		self.op(I32_NE);
		self.bail_if(next_pc, retired);
	}
}

/// Whether emit_seq translates `op` (anything else becomes a bail to the
/// interpreter). Kept beside emit_seq; a test pins the two together.
pub(crate) fn translatable(op: &BlockOp) -> bool {
	if op.kind == 0 {
		return table_op(op).is_some();
	}
	matches!(op.kind,
		HOT_ADDI | HOT_ADD | HOT_SUB | HOT_AND | HOT_OR | HOT_XOR | HOT_ANDI | HOT_ORI
		| HOT_XORI | HOT_MUL | HOT_SLL | HOT_SRL | HOT_SRA | HOT_SLLI | HOT_SRLI | HOT_SRAI
		| HOT_LUI | HOT_AUIPC | HOT_ADDIW | HOT_ADDW | HOT_SUBW | HOT_SLLIW | HOT_SRLIW
		| HOT_SRAIW | HOT_SLLW | HOT_SRLW | HOT_SRAW | HOT_SLT | HOT_SLTU | HOT_SLTI
		| HOT_SLTIU | HOT_LD | HOT_LW | HOT_LWU | HOT_LH | HOT_LHU | HOT_LB | HOT_LBU
		| HOT_SD | HOT_SW | HOT_SH | HOT_SB | HOT_BEQ | HOT_BNE | HOT_BLT | HOT_BGE
		| HOT_BLTU | HOT_BGEU | HOT_JAL | HOT_JALR | HOT_FLD | HOT_FLW | HOT_FSD | HOT_FSW
		| HOT_FADD_D | HOT_FSUB_D | HOT_FMUL_D | HOT_FDIV_D | HOT_FSGNJ_D | HOT_FMV_X_D
		| HOT_FMV_D_X | HOT_FCVT_D_W)
}

/// The non-hot ops the translator takes over from the table path, by the
/// INSTRUCTIONS entry the interpreter decoded them to. Every one of them is
/// the LAST op of its block (build_block ends a block at any non-hot op), so
/// translating one is what lets a region run on into the next block instead
/// of handing back to the interpreter there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableOp {
	Div,
	Divu,
	Rem,
	Remu,
	Mulw,
	Divw,
	Divuw,
	Remw,
	Remuw,
	Fence,
	/// the high 64 bits of the 128-bit product: (rs1 signed, rs2 signed)
	Mulh(bool, bool),
	/// AMO*: the operation, and whether it is the .W form
	Amo(AmoOp, bool),
	/// LR / SC: whether it is the .W form
	Lr(bool),
	Sc(bool),
	/// a single-precision op (the low 32 bits of the f registers)
	Fs(FOp),
	/// a double-precision op the hot set leaves to the table
	Fd(FOp),
	CvtDS,
	CvtSD,
	MvXW,
	MvWX,
	/// CSRRW/S/C and their I forms on fflags (1), frm (2) or fcsr (3) only:
	/// pure state, no privilege check that can fail, no interrupt to re-arm
	Csr(CsrOp, bool, u16),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AmoOp {
	Add,
	Swap,
	Xor,
	Or,
	And,
	Min,
	Max,
	MinU,
	MaxU,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FOp {
	Add,
	Sub,
	Mul,
	Div,
	Sqrt,
	Sgnj,
	Sgnjn,
	Sgnjx,
	Eq,
	Lt,
	Le,
	/// float -> int (FCVT.W/WU/L/LU.x)
	ToW,
	ToWu,
	ToL,
	ToLu,
	/// int -> float (FCVT.x.W/WU/L/LU)
	FromW,
	FromWu,
	FromL,
	FromLu,
	Madd,
	Msub,
	Nmsub,
	Nmadd,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CsrOp {
	W,
	S,
	C,
}

fn table_op(op: &BlockOp) -> Option<TableOp> {
	use self::AmoOp as A;
	use self::FOp as F;
	let csr = ((op.word >> 20) & 0xfff) as u16;
	let fcsr_family = (1..=3).contains(&csr);
	Some(match ::cpu::op_name(op) {
		"DIV" => TableOp::Div,
		"DIVU" => TableOp::Divu,
		"REM" => TableOp::Rem,
		"REMU" => TableOp::Remu,
		"MULW" => TableOp::Mulw,
		"DIVW" => TableOp::Divw,
		"DIVUW" => TableOp::Divuw,
		"REMW" => TableOp::Remw,
		"REMUW" => TableOp::Remuw,
		// the interpreter's FENCE and FENCE.I do nothing (one hart; the
		// write snoop already keeps cached code coherent)
		"FENCE" | "FENCE.I" => TableOp::Fence,
		"MULH" => TableOp::Mulh(true, true),
		"MULHSU" => TableOp::Mulh(true, false),
		"MULHU" => TableOp::Mulh(false, false),
		"AMOADD.W" => TableOp::Amo(A::Add, true),
		"AMOADD.D" => TableOp::Amo(A::Add, false),
		"AMOSWAP.W" => TableOp::Amo(A::Swap, true),
		"AMOSWAP.D" => TableOp::Amo(A::Swap, false),
		"AMOXOR.W" => TableOp::Amo(A::Xor, true),
		"AMOXOR.D" => TableOp::Amo(A::Xor, false),
		"AMOOR.W" => TableOp::Amo(A::Or, true),
		"AMOOR.D" => TableOp::Amo(A::Or, false),
		"AMOAND.W" => TableOp::Amo(A::And, true),
		"AMOAND.D" => TableOp::Amo(A::And, false),
		"AMOMIN.W" => TableOp::Amo(A::Min, true),
		"AMOMIN.D" => TableOp::Amo(A::Min, false),
		"AMOMAX.W" => TableOp::Amo(A::Max, true),
		"AMOMAX.D" => TableOp::Amo(A::Max, false),
		"AMOMINU.W" => TableOp::Amo(A::MinU, true),
		"AMOMINU.D" => TableOp::Amo(A::MinU, false),
		"AMOMAXU.W" => TableOp::Amo(A::MaxU, true),
		"AMOMAXU.D" => TableOp::Amo(A::MaxU, false),
		"LR.W" => TableOp::Lr(true),
		"LR.D" => TableOp::Lr(false),
		"SC.W" => TableOp::Sc(true),
		"SC.D" => TableOp::Sc(false),
		"FADD.S" => TableOp::Fs(F::Add),
		"FSUB.S" => TableOp::Fs(F::Sub),
		"FMUL.S" => TableOp::Fs(F::Mul),
		"FDIV.S" => TableOp::Fs(F::Div),
		"FSQRT.S" => TableOp::Fs(F::Sqrt),
		"FSGNJ.S" => TableOp::Fs(F::Sgnj),
		"FSGNJN.S" => TableOp::Fs(F::Sgnjn),
		"FSGNJX.S" => TableOp::Fs(F::Sgnjx),
		"FEQ.S" => TableOp::Fs(F::Eq),
		"FLT.S" => TableOp::Fs(F::Lt),
		"FLE.S" => TableOp::Fs(F::Le),
		"FCVT.W.S" => TableOp::Fs(F::ToW),
		"FCVT.WU.S" => TableOp::Fs(F::ToWu),
		"FCVT.L.S" => TableOp::Fs(F::ToL),
		"FCVT.LU.S" => TableOp::Fs(F::ToLu),
		"FCVT.S.W" => TableOp::Fs(F::FromW),
		"FCVT.S.WU" => TableOp::Fs(F::FromWu),
		"FCVT.S.L" => TableOp::Fs(F::FromL),
		"FCVT.S.LU" => TableOp::Fs(F::FromLu),
		"FMADD.S" => TableOp::Fs(F::Madd),
		"FMSUB.S" => TableOp::Fs(F::Msub),
		"FNMSUB.S" => TableOp::Fs(F::Nmsub),
		"FNMADD.S" => TableOp::Fs(F::Nmadd),
		"FSQRT.D" => TableOp::Fd(F::Sqrt),
		"FSGNJN.D" => TableOp::Fd(F::Sgnjn),
		"FSGNJX.D" => TableOp::Fd(F::Sgnjx),
		"FEQ.D" => TableOp::Fd(F::Eq),
		"FLT.D" => TableOp::Fd(F::Lt),
		"FLE.D" => TableOp::Fd(F::Le),
		"FCVT.W.D" => TableOp::Fd(F::ToW),
		"FCVT.WU.D" => TableOp::Fd(F::ToWu),
		"FCVT.L.D" => TableOp::Fd(F::ToL),
		"FCVT.LU.D" => TableOp::Fd(F::ToLu),
		"FCVT.D.WU" => TableOp::Fd(F::FromWu),
		"FCVT.D.L" => TableOp::Fd(F::FromL),
		"FCVT.D.LU" => TableOp::Fd(F::FromLu),
		"FMADD.D" => TableOp::Fd(F::Madd),
		"FMSUB.D" => TableOp::Fd(F::Msub),
		"FNMSUB.D" => TableOp::Fd(F::Nmsub),
		"FNMADD.D" => TableOp::Fd(F::Nmadd),
		"FCVT.D.S" => TableOp::CvtDS,
		"FCVT.S.D" => TableOp::CvtSD,
		"FMV.X.W" => TableOp::MvXW,
		"FMV.W.X" => TableOp::MvWX,
		"CSRRW" if fcsr_family => TableOp::Csr(CsrOp::W, false, csr),
		"CSRRS" if fcsr_family => TableOp::Csr(CsrOp::S, false, csr),
		"CSRRC" if fcsr_family => TableOp::Csr(CsrOp::C, false, csr),
		"CSRRWI" if fcsr_family => TableOp::Csr(CsrOp::W, true, csr),
		"CSRRSI" if fcsr_family => TableOp::Csr(CsrOp::S, true, csr),
		"CSRRCI" if fcsr_family => TableOp::Csr(CsrOp::C, true, csr),
		_ => return None,
	})
}

/// One table op, translated: each arm reproduces its INSTRUCTIONS closure
/// exactly - including the interpreter's own quirks, which are what the
/// guest has been running all along. `addr` is the op's module pc, `next`
/// the one after it; a memory op that cannot take the fast path bails at
/// `addr` having retired `ret_before`, before any of its effects.
fn emit_table_op(e: &mut Emit, t: TableOp, op: &BlockOp, addr: u64, next: u64, ret_before: u64, ret_after: u64) {
	let (rd, rs1, rs2) = (op.rd, op.rs1, op.rs2);
	match t {
		TableOp::Mulh(s1, s2) => mulh(e, rd, rs1, rs2, s1, s2),
		TableOp::Amo(a, w) => amo(e, a, w, rd, rs1, rs2, addr, next, ret_before, ret_after),
		TableOp::Lr(w) => lr(e, w, rd, rs1, addr, ret_before),
		TableOp::Sc(w) => sc(e, w, rd, rs1, rs2, addr, next, ret_before, ret_after),
		TableOp::Fs(f) => fp_single(e, f, rd, rs1, rs2, ((op.word >> 27) & 0x1f) as u8),
		TableOp::Fd(f) => fp_double(e, f, rd, rs1, rs2, ((op.word >> 27) & 0x1f) as u8),
		TableOp::CvtDS => {
			// f32::from_bits(low half) as f64
			e.set_f_pre(rd);
			e.get_f32(rs1);
			e.op(0xbb); // f64.promote_f32
			e.set_f_post(rd);
		}
		TableOp::CvtSD => {
			// the interpreter NaN-boxes this one (and only this one):
			// 0xffff_ffff_0000_0000 | (f as f32).to_bits()
			e.set_f_pre(rd);
			e.get_f(rs1);
			e.op(0xb6); // f32.demote_f64
			e.op(0xbc); // i32.reinterpret_f32
			e.op(I64_EXTEND_I32_U);
			e.i64c(0xffff_ffff_0000_0000u64 as i64);
			e.op(I64_OR);
			e.set_f_bits_post(rd);
		}
		TableOp::MvXW => {
			// x = bits as i32 as i64
			e.set_x_pre(rd);
			e.get_f_bits(rs1);
			wrap32(e);
			e.set_x_post(rd);
		}
		TableOp::MvWX => {
			// f = x as u32 as u64
			e.set_f_pre(rd);
			e.get_x(rs1);
			e.i64c(0xffff_ffff);
			e.op(I64_AND);
			e.set_f_bits_post(rd);
		}
		TableOp::Csr(kind, imm, csr) => csr_op(e, kind, imm, csr, rd, rs1),
		_ => emit_int_table_op(e, t, rd, rs1, rs2),
	}
}

const I64_XOR: u8 = 0x85;
const I64_MUL: u8 = 0x7e;
const I64_SHR_S: u8 = 0x87;
const SELECT: u8 = 0x1b;
const DROP: u8 = 0x1a;

/// MULH / MULHSU / MULHU: the high half of the 128-bit product, which wasm
/// has no instruction for. From 32-bit limbs (each partial product fits in
/// 64 bits): with a = ah:al, b = bh:bl,
///   carry = ((al*bl >> 32) + lo(al*bh) + lo(ah*bl)) >> 32
///   hi_u  = ah*bh + (al*bh >> 32) + (ah*bl >> 32) + carry
/// then two's complement turns the unsigned high half into the signed one:
/// hi_s = hi_u - (a < 0 ? b : 0) - (b < 0 ? a : 0), each term only for an
/// operand that is signed.
fn mulh(e: &mut Emit, rd: u8, rs1: u8, rs2: u8, s1: bool, s2: bool) {
	let (a, b, c) = (e.l.t0, e.l.t1, e.l.t2);
	let lo = |e: &mut Emit, l: u32| {
		e.lget(l);
		e.i64c(0xffff_ffff);
		e.op(I64_AND);
	};
	let hi = |e: &mut Emit, l: u32| {
		e.lget(l);
		e.i64c(32);
		e.op(I64_SHR_U);
	};
	e.get_x(rs1);
	e.lset(a);
	e.get_x(rs2);
	e.lset(b);
	// c = al*bl >> 32
	lo(e, a);
	lo(e, b);
	e.op(I64_MUL);
	e.i64c(32);
	e.op(I64_SHR_U);
	// + lo(al*bh) + lo(ah*bl), then >> 32: the carry into the high half
	lo(e, a);
	hi(e, b);
	e.op(I64_MUL);
	e.i64c(0xffff_ffff);
	e.op(I64_AND);
	e.op(I64_ADD);
	hi(e, a);
	lo(e, b);
	e.op(I64_MUL);
	e.i64c(0xffff_ffff);
	e.op(I64_AND);
	e.op(I64_ADD);
	e.i64c(32);
	e.op(I64_SHR_U);
	e.lset(c);
	e.set_x_pre(rd);
	hi(e, a);
	hi(e, b);
	e.op(I64_MUL);
	lo(e, a);
	hi(e, b);
	e.op(I64_MUL);
	e.i64c(32);
	e.op(I64_SHR_U);
	e.op(I64_ADD);
	hi(e, a);
	lo(e, b);
	e.op(I64_MUL);
	e.i64c(32);
	e.op(I64_SHR_U);
	e.op(I64_ADD);
	e.lget(c);
	e.op(I64_ADD);
	if s1 {
		// - (a < 0 ? b : 0)
		e.lget(a);
		e.i64c(63);
		e.op(I64_SHR_S);
		e.lget(b);
		e.op(I64_AND);
		e.op(I64_SUB);
	}
	if s2 {
		e.lget(b);
		e.i64c(63);
		e.op(I64_SHR_S);
		e.lget(a);
		e.op(I64_AND);
		e.op(I64_SUB);
	}
	e.set_x_post(rd);
}

/// AMO*.W/.D at x[rs1] (no offset): old = load; store f(x[rs2], old);
/// x[rd] = old (a .W value sign-extended). The address goes through the
/// WRITE path - TLB write way, an owned chunk, an unmarked page, outside
/// the bookkeeping windows - so a bail happens before the load and the
/// interpreter redoes the whole op; a write-way hit implies the page reads
/// too (RISC-V has no write-only pages), and an owned chunk's read and
/// write pointers are one buffer.
#[allow(clippy::too_many_arguments)]
fn amo(e: &mut Emit, a: AmoOp, w: bool, rd: u8, rs1: u8, rs2: u8, addr: u64, next: u64, ret_before: u64, ret_after: u64) {
	let (old, src, lin) = (e.l.t0, e.l.t1, e.l.addr);
	let width = if w { 4 } else { 8 };
	e.get_x(rs1);
	e.dram_addr(width, addr, ret_before, true);
	e.op(DROP); // also in `addr`
	e.lget(lin);
	match w {
		true => {
			e.op(0x34); // i64.load32_s
			e.memarg(2, 0);
		}
		false => {
			e.op(I64_LOAD);
			e.memarg(3, 0);
		}
	}
	e.lset(old);
	// x[rs2] before rd is written (rd may be rs2)
	e.get_x(rs2);
	e.lset(src);
	e.lget(lin);
	// the value stored: f(src, old)
	let pick = |e: &mut Emit, cmp32: u8, cmp64: u8| {
		e.lget(src);
		e.lget(old);
		e.lget(src);
		if w {
			e.op(I32_WRAP_I64);
		}
		e.lget(old);
		if w {
			e.op(I32_WRAP_I64);
		}
		e.op(if w { cmp32 } else { cmp64 });
		e.op(SELECT); // cond ? src : old
	};
	match a {
		AmoOp::Add => {
			e.lget(src);
			e.lget(old);
			e.op(I64_ADD);
		}
		AmoOp::Swap => e.lget(src),
		AmoOp::Xor => {
			e.lget(src);
			e.lget(old);
			e.op(I64_XOR);
		}
		AmoOp::Or => {
			e.lget(src);
			e.lget(old);
			e.op(I64_OR);
		}
		AmoOp::And => {
			e.lget(src);
			e.lget(old);
			e.op(I64_AND);
		}
		AmoOp::Min => pick(e, 0x48, 0x53), // lt_s
		AmoOp::Max => pick(e, 0x4a, 0x55), // gt_s
		AmoOp::MinU => pick(e, 0x49, 0x54), // lt_u
		AmoOp::MaxU => pick(e, 0x4b, 0x56), // gt_u
	}
	match w {
		true => {
			e.op(0x3e); // i64.store32: the low half
			e.memarg(2, 0);
		}
		false => {
			e.op(I64_STORE);
			e.memarg(3, 0);
		}
	}
	e.set_x_pre(rd);
	e.lget(old);
	e.set_x_post(rd);
	// the op is complete (rd written) before the flat-RAM generation check
	e.gen_check(next, ret_after);
}

/// LR.W/.D: x[rd] = load(x[rs1]) (.W sign-extended), and the reservation is
/// set to that (old) x[rs1].
fn lr(e: &mut Emit, w: bool, rd: u8, rs1: u8, addr: u64, ret_before: u64) {
	let (va, base) = (e.l.t0, e.l.base);
	let (flag, held) = (e.lay.res_flag_addr, e.lay.res_addr_addr);
	e.get_x(rs1);
	e.lset(va);
	e.set_x_pre(rd);
	e.lget(va);
	e.dram_addr(if w { 4 } else { 8 }, addr, ret_before, false);
	match w {
		true => {
			e.op(0x34);
			e.memarg(2, 0);
		}
		false => {
			e.op(I64_LOAD);
			e.memarg(3, 0);
		}
	}
	e.set_x_post(rd);
	e.lget(base);
	e.i32c(1);
	e.op(0x3a); // i32.store8
	e.memarg(0, flag);
	e.lget(base);
	e.lget(va);
	e.op(I64_STORE);
	e.memarg(3, held);
}

/// SC.W/.D: when the reservation is held for exactly x[rs1], store x[rs2]
/// there, drop the reservation, x[rd] = 0; otherwise drop it, x[rd] = 1. A
/// store that cannot take the fast path bails BEFORE the reservation is
/// touched, so the interpreter's retry still sees it held.
#[allow(clippy::too_many_arguments)]
fn sc(e: &mut Emit, w: bool, rd: u8, rs1: u8, rs2: u8, addr: u64, next: u64, ret_before: u64, ret_after: u64) {
	const I32_AND: u8 = 0x71;
	let (va, base) = (e.l.t0, e.l.base);
	let (flag, held) = (e.lay.res_flag_addr, e.lay.res_addr_addr);
	let clear = |e: &mut Emit| {
		e.lget(base);
		e.i32c(0);
		e.op(0x3a); // i32.store8
		e.memarg(0, flag);
	};
	e.get_x(rs1);
	e.lset(va);
	e.lget(base);
	e.op(I32_LOAD8_U);
	e.memarg(0, flag);
	e.lget(base);
	e.op(I64_LOAD);
	e.memarg(3, held);
	e.lget(va);
	e.op(I64_EQ);
	e.op(I32_AND);
	e.op(IF);
	e.op(VOID);
	e.if_depth += 1;
	e.lget(va);
	e.dram_addr(if w { 4 } else { 8 }, addr, ret_before, true);
	e.get_x(rs2);
	match w {
		true => {
			e.op(0x3e);
			e.memarg(2, 0);
		}
		false => {
			e.op(I64_STORE);
			e.memarg(3, 0);
		}
	}
	clear(e);
	e.set_x_pre(rd);
	e.i64c(0);
	e.set_x_post(rd);
	e.gen_check(next, ret_after);
	e.op(ELSE);
	clear(e);
	e.set_x_pre(rd);
	e.i64c(1);
	e.set_x_post(rd);
	e.if_depth -= 1;
	e.op(END);
}

/// An f32 constant.
fn f32c(e: &mut Emit, v: f32) {
	e.op(0x43);
	e.code.extend_from_slice(&v.to_bits().to_le_bytes());
}

/// An f64 constant.
fn f64c(e: &mut Emit, v: f64) {
	e.op(0x44);
	e.code.extend_from_slice(&v.to_bits().to_le_bytes());
}

/// The single-precision table ops. Each reads its operands as the low 32
/// bits of the f registers and writes its result zero-extended, the way
/// every .S closure does; conversions to integers use Rust's `as` (NaN -> 0,
/// saturating, toward zero), which is wasm's trunc_sat exactly.
fn fp_single(e: &mut Emit, f: FOp, rd: u8, rs1: u8, rs2: u8, rs3: u8) {
	match f {
		FOp::Add | FOp::Sub | FOp::Mul | FOp::Div => {
			if f == FOp::Div {
				// if b == 0.0 { set_fcsr_dz() } (either zero: -0.0 == 0.0)
				e.get_f32(rs2);
				f32c(e, 0.0);
				e.op(0x5b); // f32.eq
				e.op(IF);
				e.op(VOID);
				e.fcsr_or(0x8);
				e.op(END);
			}
			e.set_f_pre(rd);
			e.get_f32(rs1);
			e.get_f32(rs2);
			e.op(match f {
				FOp::Add => 0x92,
				FOp::Sub => 0x93,
				FOp::Mul => 0x94,
				_ => 0x95, // div
			});
			e.set_f32_post(rd);
		}
		FOp::Sqrt => {
			e.set_f_pre(rd);
			e.get_f32(rs1);
			e.op(0x91); // f32.sqrt
			e.set_f32_post(rd);
		}
		FOp::Sgnj | FOp::Sgnjn | FOp::Sgnjx => {
			// the sign from rs2 (inverted, or xored with rs1's), the rest of
			// rs1, all within the low 32 bits
			e.set_f_pre(rd);
			e.get_f_bits(rs2);
			if f == FOp::Sgnjx {
				e.get_f_bits(rs1);
				e.op(I64_XOR);
			}
			e.i64c(0x8000_0000);
			e.op(I64_AND);
			if f == FOp::Sgnjn {
				e.i64c(0x8000_0000);
				e.op(I64_XOR);
			}
			e.get_f_bits(rs1);
			e.i64c(0x7fff_ffff);
			e.op(I64_AND);
			e.op(I64_OR);
			e.set_f_bits_post(rd);
		}
		FOp::Eq | FOp::Lt | FOp::Le => {
			e.set_x_pre(rd);
			e.get_f32(rs1);
			e.get_f32(rs2);
			e.op(match f {
				FOp::Eq => 0x5b,
				FOp::Lt => 0x5d,
				_ => 0x5f, // le
			});
			e.op(I64_EXTEND_I32_U);
			e.set_x_post(rd);
		}
		FOp::ToW | FOp::ToWu => {
			// a as i32 / a as u32, then as i32 as i64 (sign-extended)
			e.set_x_pre(rd);
			e.get_f32(rs1);
			e.fc(if f == FOp::ToW { 0 } else { 1 }); // i32.trunc_sat_f32_s/u
			e.op(0xac); // i64.extend_i32_s
			e.set_x_post(rd);
		}
		FOp::ToL | FOp::ToLu => {
			e.set_x_pre(rd);
			e.get_f32(rs1);
			e.fc(if f == FOp::ToL { 4 } else { 5 }); // i64.trunc_sat_f32_s/u
			e.set_x_post(rd);
		}
		FOp::FromW | FOp::FromWu | FOp::FromL | FOp::FromLu => {
			e.set_f_pre(rd);
			e.get_x(rs1);
			match f {
				FOp::FromW => {
					e.op(I32_WRAP_I64);
					e.op(0xb2); // f32.convert_i32_s
				}
				FOp::FromWu => {
					e.op(I32_WRAP_I64);
					e.op(0xb3); // f32.convert_i32_u
				}
				FOp::FromL => e.op(0xb4), // f32.convert_i64_s
				_ => e.op(0xb5), // f32.convert_i64_u
			}
			e.set_f32_post(rd);
		}
		FOp::Madd | FOp::Msub | FOp::Nmsub | FOp::Nmadd => {
			// unfused, as the closures compute it: a*b+c, a*b-c, -(a*b)+c,
			// -(a*b)-c
			e.set_f_pre(rd);
			e.get_f32(rs1);
			e.get_f32(rs2);
			e.op(0x94); // f32.mul
			if matches!(f, FOp::Nmsub | FOp::Nmadd) {
				e.op(0x8c); // f32.neg
			}
			e.get_f32(rs3);
			e.op(if matches!(f, FOp::Madd | FOp::Nmsub) { 0x92 } else { 0x93 });
			e.set_f32_post(rd);
		}
	}
}

/// The double-precision ops the hot set leaves to the table. Conversions
/// to integers follow the risc-box patches: saturating `as`, but NaN gives
/// the type's MAX (signed) or all ones (unsigned) - wasm's trunc_sat gives
/// 0 there, so NaN is selected separately.
fn fp_double(e: &mut Emit, f: FOp, rd: u8, rs1: u8, rs2: u8, rs3: u8) {
	match f {
		FOp::Sqrt => {
			e.set_f_pre(rd);
			e.get_f(rs1);
			e.op(0x9f); // f64.sqrt
			e.set_f_post(rd);
		}
		FOp::Sgnjn | FOp::Sgnjx => {
			e.set_f_pre(rd);
			e.get_f_bits(rs2);
			if f == FOp::Sgnjx {
				e.get_f_bits(rs1);
				e.op(I64_XOR);
			}
			e.i64c(i64::MIN);
			e.op(I64_AND);
			if f == FOp::Sgnjn {
				e.i64c(i64::MIN);
				e.op(I64_XOR);
			}
			e.get_f_bits(rs1);
			e.i64c(i64::MAX);
			e.op(I64_AND);
			e.op(I64_OR);
			e.set_f_bits_post(rd);
		}
		FOp::Eq | FOp::Lt | FOp::Le => {
			e.set_x_pre(rd);
			e.get_f(rs1);
			e.get_f(rs2);
			e.op(match f {
				FOp::Eq => 0x61,
				FOp::Lt => 0x63,
				_ => 0x65, // le
			});
			e.op(I64_EXTEND_I32_U);
			e.set_x_post(rd);
		}
		FOp::ToW | FOp::ToWu | FOp::ToL | FOp::ToLu => {
			// select(nan_value, converted, a != a)
			e.set_x_pre(rd);
			e.i64c(match f {
				FOp::ToW => i32::MAX as i64,
				FOp::ToWu => -1, // u32::MAX as i32 as i64
				FOp::ToL => i64::MAX,
				_ => -1, // u64::MAX as i64
			});
			e.get_f(rs1);
			match f {
				FOp::ToW => {
					e.fc(2); // i32.trunc_sat_f64_s
					e.op(0xac);
				}
				FOp::ToWu => {
					e.fc(3); // i32.trunc_sat_f64_u
					e.op(0xac);
				}
				FOp::ToL => e.fc(6), // i64.trunc_sat_f64_s
				_ => e.fc(7), // i64.trunc_sat_f64_u
			}
			e.get_f(rs1);
			e.get_f(rs1);
			e.op(0x62); // f64.ne: NaN
			e.op(SELECT);
			e.set_x_post(rd);
		}
		FOp::FromWu | FOp::FromL | FOp::FromLu => {
			e.set_f_pre(rd);
			e.get_x(rs1);
			match f {
				FOp::FromWu => {
					e.op(I32_WRAP_I64);
					e.op(0xb8); // f64.convert_i32_u
				}
				FOp::FromL => e.op(0xb9), // f64.convert_i64_s
				_ => e.op(0xba), // f64.convert_i64_u
			}
			e.set_f_post(rd);
		}
		FOp::Madd | FOp::Msub | FOp::Nmsub | FOp::Nmadd => {
			e.set_f_pre(rd);
			e.get_f(rs1);
			e.get_f(rs2);
			e.op(0xa2); // f64.mul
			if matches!(f, FOp::Nmsub | FOp::Nmadd) {
				e.op(0x9a); // f64.neg
			}
			e.get_f(rs3);
			e.op(if matches!(f, FOp::Madd | FOp::Nmsub) { 0xa0 } else { 0xa1 });
			e.set_f_post(rd);
		}
		// the hot set's (FADD/FSUB/FMUL/FDIV.D) and FCVT.D.W never reach here
		_ => unreachable!("not a table double op"),
	}
}

/// CSRRW/S/C (and I forms) on fflags, frm, fcsr, as the closures run them:
/// data = read; src = x[rs1] (taken before rd is written) or the 5-bit
/// zimm; x[rd] = data; write(src | data|src | data&!src). read_csr_raw
/// gives fflags = fcsr & 0x1f and frm = (fcsr >> 5) & 7; write_csr_raw
/// merges fflags and frm into fcsr and stores fcsr itself raw.
fn csr_op(e: &mut Emit, kind: CsrOp, imm: bool, csr: u16, rd: u8, rs1: u8) {
	let (data, src, val, base, at) = (e.l.t0, e.l.t1, e.l.t2, e.l.base, e.lay.fcsr_addr);
	let fcsr = |e: &mut Emit| {
		e.lget(base);
		e.op(I64_LOAD);
		e.memarg(3, at);
	};
	fcsr(e);
	match csr {
		1 => {
			e.i64c(0x1f);
			e.op(I64_AND);
		}
		2 => {
			e.i64c(5);
			e.op(I64_SHR_U);
			e.i64c(7);
			e.op(I64_AND);
		}
		_ => {}
	}
	e.lset(data);
	match imm {
		true => e.i64c(rs1 as i64),
		false => e.get_x(rs1),
	}
	e.lset(src);
	e.set_x_pre(rd);
	e.lget(data);
	e.set_x_post(rd);
	match kind {
		CsrOp::W => e.lget(src),
		CsrOp::S => {
			e.lget(data);
			e.lget(src);
			e.op(I64_OR);
		}
		CsrOp::C => {
			e.lget(data);
			e.lget(src);
			e.i64c(-1);
			e.op(I64_XOR);
			e.op(I64_AND);
		}
	}
	e.lset(val);
	e.lget(base);
	match csr {
		1 | 2 => {
			let (keep, shift, mask) = if csr == 1 { (!0x1fi64, 0, 0x1f) } else { (!0xe0i64, 5, 0xe0) };
			fcsr(e);
			e.i64c(keep);
			e.op(I64_AND);
			e.lget(val);
			if shift != 0 {
				e.i64c(shift);
				e.op(I64_SHL);
			}
			e.i64c(mask);
			e.op(I64_AND);
			e.op(I64_OR);
		}
		_ => e.lget(val),
	}
	e.op(I64_STORE);
	e.memarg(3, at);
}

/// The integer table ops, in wasm (RV64): rd = f(x[rs1], x[rs2]) with
/// RISC-V's division rules — x/0 = -1 (all ones), x%0 = x, MIN/-1 = MIN,
/// MIN%-1 = 0 — guarded before wasm's trapping div/rem.
fn emit_int_table_op(e: &mut Emit, t: TableOp, rd: u8, rs1: u8, rs2: u8) {
	const I32_EQZ: u8 = 0x45;
	const I32_EQ: u8 = 0x46;
	const I32_AND: u8 = 0x71;
	const IF_I64: u8 = 0x7e;
	if t == TableOp::Fence {
		return;
	}
	e.set_x_pre(rd);
	match t {
		TableOp::Mulw => {
			e.get_x(rs1);
			e.get_x(rs2);
			e.op(0x7e); // i64.mul: the low 32 bits are the 32-bit product
			wrap32(e);
		}
		TableOp::Div | TableOp::Rem | TableOp::Divu | TableOp::Remu => {
			let signed = matches!(t, TableOp::Div | TableOp::Rem);
			let rem = matches!(t, TableOp::Rem | TableOp::Remu);
			e.get_x(rs2);
			e.op(I64_EQZ);
			e.op(IF);
			e.op(IF_I64);
			match rem {
				true => e.get_x(rs1),
				false => e.i64c(-1),
			}
			e.op(ELSE);
			if signed {
				e.get_x(rs1);
				e.i64c(i64::MIN);
				e.op(I64_EQ);
				e.get_x(rs2);
				e.i64c(-1);
				e.op(I64_EQ);
				e.op(I32_AND);
				e.op(IF);
				e.op(IF_I64);
				match rem {
					true => e.i64c(0),
					false => e.i64c(i64::MIN),
				}
				e.op(ELSE);
			}
			e.get_x(rs1);
			e.get_x(rs2);
			e.op(match (signed, rem) {
				(true, false) => 0x7f, // i64.div_s
				(false, false) => 0x80, // i64.div_u
				(true, true) => 0x81, // i64.rem_s
				(false, true) => 0x82, // i64.rem_u
			});
			if signed {
				e.op(END);
			}
			e.op(END);
		}
		TableOp::Divw | TableOp::Remw | TableOp::Divuw | TableOp::Remuw => {
			let signed = matches!(t, TableOp::Divw | TableOp::Remw);
			let rem = matches!(t, TableOp::Remw | TableOp::Remuw);
			e.get_x(rs2);
			e.op(I32_WRAP_I64);
			e.op(I32_EQZ);
			e.op(IF);
			e.op(IF_I64);
			match rem {
				true => {
					// x % 0 = the 32-bit dividend, sign-extended
					e.get_x(rs1);
					wrap32(e);
				}
				false => e.i64c(-1),
			}
			e.op(ELSE);
			if signed {
				e.get_x(rs1);
				e.op(I32_WRAP_I64);
				e.i32c(i32::MIN);
				e.op(I32_EQ);
				e.get_x(rs2);
				e.op(I32_WRAP_I64);
				e.i32c(-1);
				e.op(I32_EQ);
				e.op(I32_AND);
				e.op(IF);
				e.op(IF_I64);
				match rem {
					true => e.i64c(0),
					false => e.i64c(i32::MIN as i64),
				}
				e.op(ELSE);
			}
			e.get_x(rs1);
			e.op(I32_WRAP_I64);
			e.get_x(rs2);
			e.op(I32_WRAP_I64);
			e.op(match (signed, rem) {
				(true, false) => 0x6d, // i32.div_s
				(false, false) => 0x6e, // i32.div_u
				(true, true) => 0x6f, // i32.rem_s
				(false, true) => 0x70, // i32.rem_u
			});
			e.op(0xac); // i64.extend_i32_s
			if signed {
				e.op(END);
			}
			e.op(END);
		}
		_ => unreachable!(),
	}
	e.set_x_post(rd);
}

#[cfg(feature = "codegen")]
/// The cache key of a region's module, computed from its SOURCE (module
/// pcs and ops) and the layout's hash without emitting it: two hashes of
/// the same bytes the emitter is a pure function of.
pub(crate) fn source_key(blocks: &[(u64, Vec<BlockOp>)], layout_hash: u64) -> (u64, u64, u64) {
	use std::hash::Hasher;
	let mut sip = std::collections::hash_map::DefaultHasher::new();
	for &(pc, ref ops) in blocks {
		sip.write_u64(pc);
		sip.write_usize(ops.len());
		for op in ops {
			sip.write_i32(op.imm);
			sip.write_u32(op.word);
			sip.write_u16(op.data);
			sip.write(&[op.kind, op.rd, op.rs1, op.rs2, op.len]);
		}
	}
	(layout_hash, hash_blocks(blocks), sip.finish())
}

#[cfg(feature = "codegen")]
/// A stable hash of everything in a Layout the emitted code depends on.
pub fn layout_hash(lay: &Layout) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut h = std::collections::hash_map::DefaultHasher::new();
	lay.hash(&mut h);
	h.finish()
}

/// The shared per-op emission: the whole sequence plus its fallthrough
/// exit. Returns false only in single-block mode when the FIRST op is
/// unsupported (nothing to compile); region mode emits a bail stub
/// instead so the dispatcher interprets that block.
fn emit_seq(e: &mut Emit, ops: &[BlockOp], start: u64) -> bool {
	let mut pc = start;
	for (i, op) in ops.iter().enumerate() {
		let addr = pc;
		let next = addr.wrapping_add(op.len as u64);
		let ret_before = i as u64; // retired if we bail before this op
		let ret_after = i as u64 + 1; // retired if this op completes/exits
		let rd = op.rd;
		let rs1 = op.rs1;
		let rs2 = op.rs2;
		let imm = op.imm as i64;
		match op.kind {
			HOT_ADDI => bin_imm(e, rd, rs1, imm, 0x7c),
			HOT_ADD => bin_reg(e, rd, rs1, rs2, 0x7c),
			HOT_SUB => bin_reg(e, rd, rs1, rs2, 0x7d),
			HOT_AND => bin_reg(e, rd, rs1, rs2, 0x83),
			HOT_OR => bin_reg(e, rd, rs1, rs2, 0x84),
			HOT_XOR => bin_reg(e, rd, rs1, rs2, 0x85),
			HOT_ANDI => bin_imm(e, rd, rs1, imm, 0x83),
			HOT_ORI => bin_imm(e, rd, rs1, imm, 0x84),
			HOT_XORI => bin_imm(e, rd, rs1, imm, 0x85),
			HOT_MUL => bin_reg(e, rd, rs1, rs2, 0x7e),
			HOT_SLL => bin_reg(e, rd, rs1, rs2, 0x86),
			HOT_SRL => bin_reg(e, rd, rs1, rs2, 0x88),
			HOT_SRA => bin_reg(e, rd, rs1, rs2, 0x87),
			HOT_SLLI => shift_imm(e, rd, rs1, op.word, 0x86),
			HOT_SRLI => shift_imm(e, rd, rs1, op.word, 0x88),
			HOT_SRAI => shift_imm(e, rd, rs1, op.word, 0x87),
			HOT_LUI => {
				e.set_x_pre(rd);
				e.i64c(imm);
				e.set_x_post(rd);
			}
			HOT_AUIPC => {
				e.set_x_pre(rd);
				e.push_pc(addr.wrapping_add(imm as u64));
				e.set_x_post(rd);
			}
			HOT_ADDIW => {
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.i64c(imm);
				e.op(0x7c);
				wrap32(e);
				e.set_x_post(rd);
			}
			HOT_ADDW => {
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.get_x(rs2);
				e.op(0x7c);
				wrap32(e);
				e.set_x_post(rd);
			}
			HOT_SUBW => {
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.get_x(rs2);
				e.op(0x7d);
				wrap32(e);
				e.set_x_post(rd);
			}
			HOT_SLLIW => {
				// body: (x[rs1] << shamt) as i32 as i64, shamt = rs2 field
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.i64c(rs2 as i64);
				e.op(0x86);
				wrap32(e);
				e.set_x_post(rd);
			}
			HOT_SRLIW => {
				// ((x as u32) >> shamt) as i32 as i64
				let shamt = ((op.word >> 20) & 0x3f) as i32;
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.op(0xa7); // wrap to u32
				e.i32c(shamt);
				e.op(0x76); // i32.shr_u
				e.op(0xac); // i64.extend_i32_s
				e.set_x_post(rd);
			}
			HOT_SRAIW => {
				let shamt = ((op.word >> 20) & 0x1f) as i32;
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.op(0xa7);
				e.i32c(shamt);
				e.op(0x75); // i32.shr_s
				e.op(0xac);
				e.set_x_post(rd);
			}
			HOT_SLLW => w_shift_reg(e, rd, rs1, rs2, 0x74),
			HOT_SRLW => w_shift_reg(e, rd, rs1, rs2, 0x76),
			HOT_SRAW => w_shift_reg(e, rd, rs1, rs2, 0x75),
			HOT_SLT => cmp_reg(e, rd, rs1, rs2, 0x53),
			HOT_SLTU => cmp_reg(e, rd, rs1, rs2, 0x54),
			HOT_SLTI => cmp_imm(e, rd, rs1, imm, 0x53),
			HOT_SLTIU => cmp_imm(e, rd, rs1, imm, 0x54),
			HOT_LD => load(e, rd, rs1, imm, addr, ret_before, 8, I64_LOAD, 3),
			HOT_LW => load(e, rd, rs1, imm, addr, ret_before, 4, 0x34, 2),
			HOT_LWU => load(e, rd, rs1, imm, addr, ret_before, 4, 0x35, 2),
			HOT_LH => load(e, rd, rs1, imm, addr, ret_before, 2, 0x32, 1),
			HOT_LHU => load(e, rd, rs1, imm, addr, ret_before, 2, 0x33, 1),
			HOT_LB => load(e, rd, rs1, imm, addr, ret_before, 1, 0x30, 0),
			HOT_LBU => load(e, rd, rs1, imm, addr, ret_before, 1, 0x31, 0),
			HOT_SD => {
				store(e, rs1, rs2, imm, addr, ret_before, 8, I64_STORE, 3);
				e.gen_check(next, ret_after);
			}
			HOT_SW => {
				store(e, rs1, rs2, imm, addr, ret_before, 4, 0x3e, 2);
				e.gen_check(next, ret_after);
			}
			HOT_SH => {
				store(e, rs1, rs2, imm, addr, ret_before, 2, 0x3d, 1);
				e.gen_check(next, ret_after);
			}
			HOT_SB => {
				store(e, rs1, rs2, imm, addr, ret_before, 1, 0x3c, 0);
				e.gen_check(next, ret_after);
			}
			HOT_BEQ => branch(e, rs1, rs2, 0x51, addr, imm, next, ret_after),
			HOT_BNE => branch(e, rs1, rs2, 0x52, addr, imm, next, ret_after),
			HOT_BLT => branch(e, rs1, rs2, 0x53, addr, imm, next, ret_after),
			HOT_BGE => branch(e, rs1, rs2, 0x59, addr, imm, next, ret_after),
			HOT_BLTU => branch(e, rs1, rs2, 0x54, addr, imm, next, ret_after),
			HOT_BGEU => branch(e, rs1, rs2, 0x5a, addr, imm, next, ret_after),
			HOT_JAL => {
				e.set_x_pre(rd);
				e.push_pc(next);
				e.set_x_post(rd);
				let target = addr.wrapping_add(imm as u64);
				// exec_block exits only when pc != next; a jump to the very
				// next instruction falls through there, so it must here too
				if target != next {
					e.exit(target, ret_after);
				}
			}
			HOT_JALR => jalr(e, rd, rs1, imm, next, ret_after),
			HOT_FLD => {
				// f[rd] = f64::from_bits(load_doubleword)
				e.set_f_pre(rd);
				e.get_x(rs1);
				e.i64c(imm);
				e.op(0x7c);
				e.dram_addr(8, addr, ret_before, false);
				e.op(I64_LOAD);
				e.memarg(3, 0);
				e.set_f_bits_post(rd);
			}
			HOT_FLW => {
				// f[rd] = f64::from_bits(load_word as i32 as i64 as u64)
				e.set_f_pre(rd);
				e.get_x(rs1);
				e.i64c(imm);
				e.op(0x7c);
				e.dram_addr(4, addr, ret_before, false);
				e.op(0x34); // i64.load32_s
				e.memarg(2, 0);
				e.set_f_bits_post(rd);
			}
			HOT_FSD => {
				e.get_x(rs1);
				e.i64c(imm);
				e.op(0x7c);
				e.dram_addr(8, addr, ret_before, true);
				e.get_f_bits(rs2);
				e.op(I64_STORE);
				e.memarg(3, 0);
				e.gen_check(next, ret_after);
			}
			HOT_FSW => {
				e.get_x(rs1);
				e.i64c(imm);
				e.op(0x7c);
				e.dram_addr(4, addr, ret_before, true);
				e.get_f_bits(rs2);
				e.op(0x3e); // i64.store32 (low 32 bits = to_bits() as u32)
				e.memarg(2, 0);
				e.gen_check(next, ret_after);
			}
			HOT_FADD_D => fp_bin(e, rd, rs1, rs2, 0xa0),
			HOT_FSUB_D => fp_bin(e, rd, rs1, rs2, 0xa1),
			HOT_FMUL_D => fp_bin(e, rd, rs1, rs2, 0xa2),
			HOT_FDIV_D => {
				// verbatim from the interpreter: ANY zero divisor (-0.0 ==
				// 0.0, so the -0.0 arm there never runs) gives +inf and
				// raises DZ; otherwise IEEE division
				e.set_f_pre(rd);
				e.get_f(rs2);
				f64c(e, 0.0);
				e.op(0x61); // f64.eq
				e.op(IF);
				e.op(0x7c); // -> f64
				e.fcsr_or(0x8);
				f64c(e, f64::INFINITY);
				e.op(ELSE);
				e.get_f(rs1);
				e.get_f(rs2);
				e.op(0xa3); // f64.div
				e.op(END);
				e.set_f_post(rd);
			}
			HOT_FSGNJ_D => {
				// f[rd] = (bits(rs2) & SIGN) | (bits(rs1) & !SIGN)
				e.set_f_pre(rd);
				e.get_f_bits(rs2);
				e.i64c(i64::MIN); // 0x8000...0
				e.op(0x83); // and
				e.get_f_bits(rs1);
				e.i64c(i64::MAX); // 0x7fff...f
				e.op(0x83);
				e.op(0x84); // or
				e.set_f_bits_post(rd);
			}
			HOT_FMV_X_D => {
				e.set_x_pre(rd);
				e.get_f_bits(rs1);
				e.set_x_post(rd);
			}
			HOT_FMV_D_X => {
				e.set_f_pre(rd);
				e.get_x(rs1);
				e.set_f_bits_post(rd);
			}
			HOT_FCVT_D_W => {
				// f[rd] = x[rs1] as i32 as f64 (exact conversion)
				e.set_f_pre(rd);
				e.get_x(rs1);
				e.op(0xa7); // i32.wrap_i64
				e.op(0xb7); // f64.convert_i32_s
				e.set_f_post(rd);
			}
			0 if table_op(op).is_some() => {
				emit_table_op(e, table_op(op).unwrap(), op, addr, next, ret_before, ret_after);
			}
			_ => {
				// outside the subset. A first-op miss means nothing to
				// compile (single-block mode) or a bail stub (region mode);
				// mid-block, emit a bail so the interpreter takes over at
				// exactly this op, and stop emitting.
				if i == 0 && e.strict {
					return false;
				}
				e.bail(addr, ret_before);
				return true;
			}
		}
		pc = next;
	}
	// fallthrough exit
	e.exit(pc, ops.len() as u64);
	true
}

/// JALR: tmp = next; pc = x[rs1] + imm; x[rd] = tmp (the target uses the
/// OLD rs1 when rd == rs1). Exits only when the target differs from next,
/// like exec_block. The runtime target goes to the indirect dispatcher,
/// which continues in-region when it is a member block start (returns and
/// computed jumps stay compiled) and leaves otherwise.
fn jalr(e: &mut Emit, rd: u8, rs1: u8, imm: i64, next: u64, ret_after: u64) {
	let sc = e.l.scratch;
	e.get_x(rs1);
	e.i64c(imm);
	e.op(I64_ADD);
	e.lset(sc);
	e.set_x_pre(rd);
	e.push_pc(next);
	e.set_x_post(rd);
	e.lget(sc);
	e.push_pc(next);
	e.op(I64_NE);
	e.op(IF);
	e.op(VOID);
	e.if_depth += 1;
	e.add_retired(ret_after);
	// tpc = target - bias (module-relative)
	e.lget(sc);
	e.lget(e.l.bias);
	e.op(I64_SUB);
	e.lset(e.l.tpc);
	let d = e.dispatch.expect("a region with a JALR has a dispatcher");
	e.i32c(d as i32);
	e.lset(e.l.cur);
	e.op(BR);
	let depth = e.loop_depth + e.if_depth;
	e.idx(depth as u64);
	e.if_depth -= 1;
	e.op(END); // fall through when target == next
}

/// The indirect dispatcher's search: tpc against the sorted member starts.
/// A balanced compare tree, linear at the leaves; a match sets cur and
/// re-enters the dispatch loop (whose br_table runs that block, fuel check
/// first).
fn emit_search(e: &mut Emit, pcs: &[(u64, u32)]) {
	if pcs.len() <= 4 {
		for &(p, idx) in pcs {
			e.lget(e.l.tpc);
			e.i64c(p as i64);
			e.op(I64_EQ);
			e.op(IF);
			e.op(VOID);
			e.i32c(idx as i32);
			e.lset(e.l.cur);
			e.op(BR);
			let depth = e.loop_depth + e.if_depth + 1;
			e.idx(depth as u64);
			e.op(END);
		}
		return;
	}
	let mid = pcs.len() / 2;
	e.lget(e.l.tpc);
	e.i64c(pcs[mid].0 as i64);
	e.op(I64_LT_U);
	e.op(IF);
	e.op(VOID);
	e.if_depth += 1;
	emit_search(e, &pcs[..mid]);
	e.op(ELSE);
	emit_search(e, &pcs[mid..]);
	e.if_depth -= 1;
	e.op(END);
}

/// Emit a REGION: several blocks in one module, in-region control
/// transfers compiled as branches back through a dispatch loop, exits
/// leaving pc exact like everything else. Signature:
/// run(fuel: i64, entry: i32) -> retired: i64 — the function stops at a
/// block boundary once `retired >= fuel` (device servicing keeps its
/// cadence), and `entry` picks the starting block (out of range: returns 0
/// untouched). A call can retire 0 (fuel exhausted at entry, or an
/// unsupported first op): the dispatcher must make progress another way
/// before retrying. Block pcs are module pcs: the caller rebases them and
/// supplies the difference as the context block's bias.
pub fn emit_region(blocks: &[(u64, Vec<BlockOp>)], lay: &Layout) -> Option<Vec<u8>> {
	emit_region_impl(blocks, lay, false)
}

/// One block as a region module (tests): None when its first op is not
/// translated. Called with fuel 1 it runs the block exactly once, as
/// exec_block does (a jump to its own start re-enters, meets the spent
/// fuel and leaves with pc there).
pub fn emit_block(ops: &[BlockOp], start: u64, lay: &Layout) -> Option<Vec<u8>> {
	emit_region_impl(&[(start, ops.to_vec())], lay, true)
}

fn emit_region_impl(blocks: &[(u64, Vec<BlockOp>)], lay: &Layout, strict: bool) -> Option<Vec<u8>> {
	if blocks.is_empty() || blocks.len() > 512 || (lay.shared && lay.max_pages.is_none()) {
		return None;
	}
	let n = blocks.len();
	let mut e = Emit::new(lay);
	e.strict = strict;
	for (i, &(start, _)) in blocks.iter().enumerate() {
		e.targets.entry(start).or_insert(i as u32);
	}
	let has_jalr = blocks.iter().any(|(_, ops)| ops.iter().any(|o| o.kind == HOT_JALR));
	let labels = n + has_jalr as usize;
	e.dispatch = if has_jalr { Some(n as u32) } else { None };
	e.plan_registers(blocks);
	let l = e.l;
	// entry guard: an index the module does not have runs nothing
	e.lget(l.entry);
	e.i32c(n as i32);
	e.op(I32_GE_U);
	e.op(IF);
	e.op(VOID);
	e.i64c(0);
	e.op(RETURN);
	e.op(END);
	e.prologue();
	e.lget(l.entry);
	e.lset(l.cur);
	e.op(BLOCK); // $exit
	e.op(VOID);
	// dispatch loop: br_table on cur into one label per block (+ the
	// indirect dispatcher)
	e.op(LOOP);
	e.op(VOID);
	for _ in 0..labels {
		e.op(BLOCK);
		e.op(VOID);
	}
	e.lget(l.cur);
	e.op(BR_TABLE);
	e.idx(labels as u64);
	for i in 0..labels {
		e.idx(i as u64);
	}
	e.idx(0); // default: block 0 (unreachable: entry is guarded)
	for (i, &(start, ref ops)) in blocks.iter().enumerate() {
		e.op(END); // end of label B_i; code for block i follows
		e.loop_depth = (labels - 1 - i) as u32;
		// fuel check: retired >= fuel -> leave with pc = start
		e.lget(l.retired);
		e.lget(l.fuel);
		e.op(I64_GE_U);
		e.op(IF);
		e.op(VOID);
		e.if_depth += 1;
		e.to_exit(start);
		e.if_depth -= 1;
		e.op(END);
		if !emit_seq(&mut e, ops, start) {
			return None;
		}
	}
	if has_jalr {
		e.op(END); // label n: the indirect dispatcher, directly in the loop
		e.loop_depth = 0;
		let mut sorted: Vec<(u64, u32)> = e.targets.iter().map(|(&p, &i)| (p, i)).collect();
		sorted.sort();
		emit_search(&mut e, &sorted);
		// no member starts at the target: leave with pc exact
		e.lget(l.tpc);
		e.lget(l.bias);
		e.op(I64_ADD);
		e.lset(l.pcv);
		e.op(BR);
		e.idx(1); // $exit
	}
	e.op(END); // end loop
	e.op(UNREACHABLE); // the loop never falls through
	e.op(END); // $exit
	e.epilogue();
	let regs = e.reg_locals;
	Some(assemble(e.code, regs, lay))
}

/// ... as i32 as i64 (wrap then sign-extend)
fn wrap32(e: &mut Emit) {
	e.op(0xa7); // i32.wrap_i64
	e.op(0xac); // i64.extend_i32_s
}

fn fp_bin(e: &mut Emit, rd: u8, rs1: u8, rs2: u8, fop: u8) {
	e.set_f_pre(rd);
	e.get_f(rs1);
	e.get_f(rs2);
	e.op(fop);
	e.set_f_post(rd);
}

fn bin_reg(e: &mut Emit, rd: u8, rs1: u8, rs2: u8, wop: u8) {
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.get_x(rs2);
	e.op(wop);
	e.set_x_post(rd);
}

fn bin_imm(e: &mut Emit, rd: u8, rs1: u8, imm: i64, wop: u8) {
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.i64c(imm);
	e.op(wop);
	e.set_x_post(rd);
}

fn shift_imm(e: &mut Emit, rd: u8, rs1: u8, word: u32, wop: u8) {
	let shamt = ((word >> 20) & 0x3f) as i64; // RV64 mask, body-exact
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.i64c(shamt);
	e.op(wop);
	e.set_x_post(rd);
}

fn w_shift_reg(e: &mut Emit, rd: u8, rs1: u8, rs2: u8, i32op: u8) {
	// (x[rs1] as u32).wrapping_shX(x[rs2] as u32) as i32 as i64
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.op(0xa7); // wrap
	e.get_x(rs2);
	e.op(0xa7);
	e.op(i32op); // wasm masks count by 31 = wrapping_shX semantics
	e.op(0xac); // extend_s
	e.set_x_post(rd);
}

fn cmp_reg(e: &mut Emit, rd: u8, rs1: u8, rs2: u8, cmp: u8) {
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.get_x(rs2);
	e.op(cmp);
	e.op(0xad); // extend_i32_u (0/1)
	e.set_x_post(rd);
}

fn cmp_imm(e: &mut Emit, rd: u8, rs1: u8, imm: i64, cmp: u8) {
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.i64c(imm);
	e.op(cmp);
	e.op(0xad);
	e.set_x_post(rd);
}

fn load(e: &mut Emit, rd: u8, rs1: u8, imm: i64, addr: u64, ret: u64, width: u64, lop: u8, align: u8) {
	e.set_x_pre(rd);
	e.get_x(rs1);
	e.i64c(imm);
	e.op(0x7c); // guest addr
	e.dram_addr(width, addr, ret, false);
	e.op(lop);
	e.memarg(align, 0);
	e.set_x_post(rd);
}

fn store(e: &mut Emit, rs1: u8, rs2: u8, imm: i64, addr: u64, ret: u64, width: u64, sop: u8, align: u8) {
	e.get_x(rs1);
	e.i64c(imm);
	e.op(0x7c);
	e.dram_addr(width, addr, ret, true);
	e.get_x(rs2);
	e.op(sop);
	e.memarg(align, 0);
}

fn branch(e: &mut Emit, rs1: u8, rs2: u8, cmp: u8, addr: u64, imm: i64, next: u64, ret: u64) {
	let target = addr.wrapping_add(imm as u64);
	// a taken branch to the very next instruction is a fallthrough in
	// exec_block's contract (it exits only when pc != next)
	if target == next {
		return;
	}
	e.get_x(rs1);
	e.get_x(rs2);
	e.op(cmp);
	e.op(IF);
	e.op(VOID);
	e.if_depth += 1;
	e.exit(target, ret);
	e.if_depth -= 1;
	e.op(END);
}

fn section(m: &mut Vec<u8>, id: u8, body: &[u8]) {
	m.push(id);
	uleb(m, body.len() as u64);
	m.extend_from_slice(body);
}

fn name(out: &mut Vec<u8>, s: &str) {
	uleb(out, s.len() as u64);
	out.extend_from_slice(s.as_bytes());
}

/// One function, importing exactly `env.memory` and exporting `run` — the
/// shape enclave:codegen accepts (no start, tables, globals, data or
/// element segments).
fn assemble(body_expr: Vec<u8>, reg_locals: u32, lay: &Layout) -> Vec<u8> {
	let mut m = vec![0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0];
	let mut s = Vec::new();
	uleb(&mut s, 1);
	s.extend_from_slice(&[0x60, 2, 0x7e, 0x7f, 1, 0x7e]); // (i64, i32) -> i64
	section(&mut m, 1, &s);
	let mut s = Vec::new();
	uleb(&mut s, 1);
	name(&mut s, "env");
	name(&mut s, "memory");
	s.push(0x02);
	let flags = lay.max_pages.is_some() as u8
		| if lay.shared { 2 } else { 0 }
		| if lay.memory64 { 4 } else { 0 };
	s.push(flags);
	uleb(&mut s, 0);
	if let Some(max) = lay.max_pages {
		uleb(&mut s, max);
	}
	section(&mut m, 2, &s);
	section(&mut m, 3, &[1, 0]);
	let mut s = Vec::new();
	uleb(&mut s, 1);
	name(&mut s, "run");
	s.extend_from_slice(&[0x00, 0]);
	section(&mut m, 7, &s);
	let addr = if lay.memory64 { 0x7e } else { 0x7f };
	let mut body = Vec::new();
	uleb(&mut body, if reg_locals > 0 { 7 } else { 6 });
	body.extend_from_slice(&[1, 0x7e]); // 2 scratch
	body.extend_from_slice(&[1, 0x7f]); // 3 cur
	body.extend_from_slice(&[5, 0x7e]); // 4 retired, 5 scratch2, 6 bias, 7 tpc, 8 pcv
	body.extend_from_slice(&[5, addr]); // 9 base, 10 rdt, 11 wrt, 12 marks, 13 addr
	body.extend_from_slice(&[1, 0x7f]); // 14 meta
	body.extend_from_slice(&[3, 0x7e]); // 15 t0, 16 t1, 17 t2
	if reg_locals > 0 {
		uleb(&mut body, reg_locals as u64); // 15.. cached guest registers
		body.push(0x7e);
	}
	body.extend_from_slice(&body_expr);
	body.push(END);
	let mut s = Vec::new();
	uleb(&mut s, 1);
	uleb(&mut s, body.len() as u64);
	s.extend_from_slice(&body);
	section(&mut m, 10, &s);
	m
}

// ---- region formation ----------------------------------------------------

/// Pick compilation regions from a recorded block graph: nodes are block
/// start pcs with a heat (retired instructions), edges are observed
/// block-to-block successions. Only LOCAL edges (|Δpc| <= 64K) join blocks
/// into a region — calls and returns collapse a whole program into one
/// hairball otherwise, and a region compiler doesn't cross them (compiled
/// units reach each other through the dispatcher instead). Regions are
/// function-local SCCs (loops), ranked by heat, each capped at
/// `max_blocks` by dropping its coldest members; singleton nodes without a
/// self-loop are not regions (a lone block is the single-block emitter's
/// job).
/// Greedy trace-growing formation: seed at the hottest unclaimed block and
/// grow along the heaviest observed edges — across calls and returns, not
/// just function-local branches. Loop SCCs come out as loops anyway (their
/// edges dominate), and call-shaped hot paths (renderer -> helpers) become
/// one region instead of stopping at every JAL. A function shared by many
/// callers still compiles: the hot caller's return edge is in-region, the
/// cold callers' exits fall back to the interpreter.
pub fn form_regions_greedy(
	nodes: &[(u64, u64)],
	edges: &[(u64, u64, u64)],
	max_blocks: usize,
	min_heat: u64,
) -> Vec<Vec<u64>> {
	use std::collections::BinaryHeap;
	let heat: ::fnv::FnvHashMap<u64, u64> = nodes.iter().cloned().collect();
	// Only edges between blocks that have heat this pass can join a region;
	// self-loops are a set (a lone block is a region only if it loops).
	let mut adj: ::fnv::FnvHashMap<u64, Vec<(u64, u64)>> = Default::default();
	let mut self_loop: ::fnv::FnvHashSet<u64> = Default::default();
	for &(a, b, w) in edges {
		if a == b {
			self_loop.insert(a);
			continue;
		}
		if !heat.contains_key(&a) || !heat.contains_key(&b) {
			continue;
		}
		adj.entry(a).or_default().push((b, w));
		adj.entry(b).or_default().push((a, w));
	}
	let mut order: Vec<(u64, u64)> = nodes.to_vec();
	order.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
	let mut claimed: ::fnv::FnvHashSet<u64> = Default::default();
	let mut out = Vec::new();
	for &(seed, h) in &order {
		if h < min_heat || claimed.contains(&seed) {
			continue;
		}
		// grow along the heaviest edge from any member (a max-heap frontier
		// with lazy deletion: O(E log E), not O(members^2 * degree))
		let mut members: Vec<u64> = vec![seed];
		let mut inset: ::fnv::FnvHashSet<u64> = Default::default();
		inset.insert(seed);
		let mut frontier: BinaryHeap<(u64, u64)> = BinaryHeap::new();
		let push = |frontier: &mut BinaryHeap<(u64, u64)>, at: u64, inset: &::fnv::FnvHashSet<u64>,
			claimed: &::fnv::FnvHashSet<u64>| {
			if let Some(nb) = adj.get(&at) {
				for &(pc, w) in nb {
					if !inset.contains(&pc) && !claimed.contains(&pc) {
						frontier.push((w, pc));
					}
				}
			}
		};
		push(&mut frontier, seed, &inset, &claimed);
		while members.len() < max_blocks {
			let pc = match frontier.pop() {
				Some((_, pc)) => pc,
				None => break,
			};
			if inset.contains(&pc) || claimed.contains(&pc) {
				continue;
			}
			members.push(pc);
			inset.insert(pc);
			push(&mut frontier, pc, &inset, &claimed);
		}
		if members.len() < 2 && !self_loop.contains(&seed) {
			continue; // a lone block with no self-loop is not a region
		}
		members.sort();
		for m in &members {
			claimed.insert(*m);
		}
		out.push(members);
	}
	out
}

pub fn form_regions(
	nodes: &[(u64, u64)],
	edges: &[(u64, u64)],
	max_blocks: usize,
	min_heat: u64,
) -> Vec<Vec<u64>> {
	use std::collections::HashMap;
	let mut ids: Vec<u64> = nodes.iter().map(|&(pc, _)| pc).collect();
	ids.sort();
	ids.dedup();
	let index_of = |pc: u64| ids.binary_search(&pc).ok();
	let heat: HashMap<u64, u64> = nodes.iter().cloned().collect();
	let n = ids.len();
	let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
	let mut self_loop = vec![false; n];
	for &(a, b) in edges {
		if a.abs_diff(b) > 0x1_0000 {
			continue;
		}
		if let (Some(ia), Some(ib)) = (index_of(a), index_of(b)) {
			if ia == ib {
				self_loop[ia] = true;
			} else {
				adj[ia].push(ib as u32);
			}
		}
	}
	// iterative Tarjan
	let mut index = vec![u32::MAX; n];
	let mut low = vec![0u32; n];
	let mut on_stack = vec![false; n];
	let mut scc_of = vec![u32::MAX; n];
	let mut stack: Vec<u32> = Vec::new();
	let mut next_index = 0u32;
	let mut scc_count = 0u32;
	let mut call: Vec<(u32, usize)> = Vec::new();
	for start in 0..n {
		if index[start] != u32::MAX {
			continue;
		}
		call.push((start as u32, 0));
		index[start] = next_index;
		low[start] = next_index;
		next_index += 1;
		stack.push(start as u32);
		on_stack[start] = true;
		while let Some(&mut (v, ref mut ei)) = call.last_mut() {
			let v = v as usize;
			if *ei < adj[v].len() {
				let w = adj[v][*ei] as usize;
				*ei += 1;
				if index[w] == u32::MAX {
					index[w] = next_index;
					low[w] = next_index;
					next_index += 1;
					stack.push(w as u32);
					on_stack[w] = true;
					call.push((w as u32, 0));
				} else if on_stack[w] {
					low[v] = low[v].min(index[w]);
				}
			} else {
				call.pop();
				if let Some(&(pv, _)) = call.last() {
					let pv = pv as usize;
					low[pv] = low[pv].min(low[v]);
				}
				if low[v] == index[v] {
					loop {
						let w = stack.pop().unwrap();
						on_stack[w as usize] = false;
						scc_of[w as usize] = scc_count;
						if w as usize == v {
							break;
						}
					}
					scc_count += 1;
				}
			}
		}
	}
	// group, filter to cyclic, rank
	let mut groups: Vec<Vec<usize>> = vec![Vec::new(); scc_count as usize];
	for i in 0..n {
		groups[scc_of[i] as usize].push(i);
	}
	let mut regions: Vec<(u64, Vec<u64>)> = Vec::new();
	for g in groups {
		let cyclic = g.len() > 1 || (g.len() == 1 && self_loop[g[0]]);
		if !cyclic {
			continue;
		}
		let mut members: Vec<(u64, u64)> = g
			.iter()
			.map(|&i| (ids[i], heat.get(&ids[i]).cloned().unwrap_or(0)))
			.collect();
		// hottest first; cap the region size by dropping the cold tail
		members.sort_by(|a, b| b.1.cmp(&a.1));
		members.truncate(max_blocks);
		let total: u64 = members.iter().map(|&(_, h)| h).sum();
		if total < min_heat {
			continue;
		}
		let mut pcs: Vec<u64> = members.into_iter().map(|(pc, _)| pc).collect();
		pcs.sort();
		regions.push((total, pcs));
	}
	regions.sort_by(|a, b| b.0.cmp(&a.0));
	regions.into_iter().map(|(_, pcs)| pcs).collect()
}

#[cfg(test)]
mod test_formation {
	use super::form_regions;

	#[test]
	fn finds_loops_ranks_and_caps() {
		// two loops: hot A<->B, cold C->D->C; a self-loop E; straight-line F->G;
		// a far "call" edge that must not merge X into A's region
		let nodes = vec![
			(0x1000, 500u64), (0x1010, 400), // A B
			(0x2000, 30), (0x2010, 20),      // C D
			(0x3000, 100),                   // E (self-loop)
			(0x4000, 900), (0x4010, 900),    // F G straight-line (no cycle)
			(0x9_0000, 1000),                // X, far away
		];
		let edges = vec![
			(0x1000, 0x1010), (0x1010, 0x1000),
			(0x2000, 0x2010), (0x2010, 0x2000),
			(0x3000, 0x3000),
			(0x4000, 0x4010),
			(0x1000, 0x9_0000), (0x9_0000, 0x1000), // far edges: excluded
		];
		let regions = form_regions(&nodes, &edges, 8, 0);
		assert_eq!(regions.len(), 3, "two loops and a self-loop");
		assert_eq!(regions[0], vec![0x1000, 0x1010], "hottest first");
		assert_eq!(regions[1], vec![0x3000]);
		assert_eq!(regions[2], vec![0x2000, 0x2010]);

		// min_heat filters the cold loop
		let regions = form_regions(&nodes, &edges, 8, 60);
		assert_eq!(regions.len(), 2);

		// size cap drops the coldest members
		let big: Vec<(u64, u64)> = (0..10u64).map(|i| (0x5000 + i * 16, 100 - i)).collect();
		let mut ring: Vec<(u64, u64)> = (0..10u64)
			.map(|i| (0x5000 + i * 16, 0x5000 + ((i + 1) % 10) * 16))
			.collect();
		ring.push((0x5000, 0x5000 + 16));
		let regions = form_regions(&big, &ring, 4, 0);
		assert_eq!(regions.len(), 1);
		assert_eq!(regions[0].len(), 4, "capped");
		assert!(regions[0].contains(&0x5000), "hottest kept");
	}
}

// ---- tier-2 dispatch ----------------------------------------------------

/// What executes compiled regions. Production implements this over the
/// platform codegen verb (compile -> table index; call_indirect; drop);
/// tests implement it however they like. The dispatcher only assumes the
/// contract shared with emit_region: run(fuel, entry_index) executes the
/// region starting at its entry block, leaves guest pc exact, and returns
/// instructions retired (possibly 0).
pub trait CodegenBackend {
	/// Compile a region module (bytes from emit_region). The entry-pc list
	/// maps region block indices to guest pcs. Returns an opaque handle,
	/// or None when compilation is unavailable/failed (the dispatcher then
	/// blacklists the region and keeps interpreting).
	fn compile(&mut self, module: &[u8], entry_pcs: &[u64]) -> Option<u32>;
	/// Same, but with the source blocks in hand. The dispatcher calls THIS;
	/// backends that only need the module bytes inherit the default. A
	/// recording backend (AOT bake pipeline) overrides it to dump the ops,
	/// which the module bytes alone cannot give back.
	fn compile_src(
		&mut self,
		_blocks: &[(u64, Vec<::cpu::BlockOp>)],
		module: &[u8],
		entry_pcs: &[u64],
	) -> Option<u32> {
		self.compile(module, entry_pcs)
	}
	/// Execute: fuel-bounded, entry by region block index.
	fn call(&mut self, handle: u32, fuel: u64, entry: u32) -> u64;
	fn drop_region(&mut self, handle: u32);
}

/// Dispatcher state: block heat, formed-region cache, blacklists.
pub struct Tier2 {
	pub backend: Box<dyn CodegenBackend>,
	// per-pc heat since the last formation pass (pc -> retired). FNV, not
	// SipHash: the live JIT records into these on the dispatch path.
	heat: ::fnv::FnvHashMap<u64, u64>,
	// observed block successions for formation
	edges: ::fnv::FnvHashMap<(u64, u64), u64>,
	prev_pc: u64,
	// compiled: entry pc -> (handle, entry index, generation at compile)
	compiled: std::collections::HashMap<u64, (u32, u32, u32)>,
	// pcs that failed to form/compile: don't retry until the next epoch
	blacklist: std::collections::HashSet<u64>,
	// retire budget between formation passes
	since_form: u64,
	pub form_interval: u64,
	pub max_blocks: usize,
	pub min_heat: u64,
	/// Greedy trace-growing formation (crosses calls) instead of
	/// function-local loop SCCs. The AOT bake uses this: 43% of the DOOM
	/// desktop's dynamic mass is call-shaped and invisible to loop SCCs.
	pub greedy: bool,
	/// When compilation fails, poison the pcs (true: the verb world, where a
	/// failed compile stays failed) or leave them for a later formation pass
	/// (false: the AOT-bake world, where a differently-shaped next formation
	/// may hash-match a baked region).
	pub blacklist_on_fail: bool,
}

impl Tier2 {
	pub fn new(backend: Box<dyn CodegenBackend>) -> Self {
		Tier2 {
			backend,
			heat: ::fnv::FnvHashMap::default(),
			edges: ::fnv::FnvHashMap::default(),
			prev_pc: 0,
			compiled: std::collections::HashMap::new(),
			blacklist: std::collections::HashSet::new(),
			since_form: 0,
			form_interval: 50_000_000,
			max_blocks: 64,
			min_heat: 1_000_000,
			greedy: false,
			blacklist_on_fail: true,
		}
	}

	/// Record one interpreted block execution (pc, retired).
	pub fn note_block(&mut self, pc: u64, retired: u64) {
		*self.heat.entry(pc).or_insert(0) += retired;
		if self.prev_pc != 0 && self.edges.len() < 1_000_000 {
			*self.edges.entry((self.prev_pc, pc)).or_insert(0) += 1;
		}
		self.prev_pc = pc;
		self.since_form += retired;
	}

	/// The interpreter took a non-block path: break the edge chain.
	pub fn note_break(&mut self) {
		self.prev_pc = 0;
	}

	/// Enough has retired since the last formation pass?
	pub fn due(&self) -> bool {
		self.since_form >= self.form_interval
	}

	/// Consume the formation clock without forming (the AOT dispatcher's
	/// heal sweep runs on this cadence and nothing else needs the pass).
	pub fn reset_form_clock(&mut self) {
		self.since_form = 0;
	}

	/// Advance the formation clock without recording heat or an edge — the
	/// sampled-recording path (recording every dispatch costs half the
	/// machine; a 1-in-8 window of true dispatch chains keeps the shape).
	pub fn note_retire(&mut self, retired: u64) {
		self.since_form += retired;
	}

	/// A compiled region for this pc, valid against the current write-snoop
	/// generation? Returns (handle, entry index).
	pub fn lookup(&mut self, pc: u64, current_gen: u32) -> Option<(u32, u32)> {
		match self.compiled.get(&pc) {
			Some(&(h, idx, gen)) if gen == current_gen => Some((h, idx)),
			Some(&(h, _, _)) => {
				// stale: drop every entry sharing the handle
				self.backend.drop_region(h);
				self.compiled.retain(|_, &mut (hh, _, _)| hh != h);
				None
			}
			None => None,
		}
	}

	/// How many entry pcs currently map to compiled regions, and how many
	/// are blacklisted — the coverage report's denominator context.
	pub fn sizes(&self) -> (usize, usize) {
		(self.compiled.len(), self.blacklist.len())
	}

	/// Every live compiled entry: (pc, handle, entry index, generation).
	/// The AOT dispatcher mirrors these into its hash-free slot array once
	/// per formation pass — per-dispatch HashMap probes with SipHash were
	/// 65% of the first AOT run's whole profile.
	pub fn compiled_entries(&self) -> Vec<(u64, u32, u32, u32)> {
		self.compiled.iter().map(|(&pc, &(h, i, g))| (pc, h, i, g)).collect()
	}

	/// Drop entries whose write-snoop generation has moved on (the lazy
	/// per-lookup drop only works when lookup runs per dispatch).
	pub fn prune_stale(&mut self, current_gen: u32) {
		let stale: Vec<u32> = self
			.compiled
			.values()
			.filter(|&&(_, _, g)| g != current_gen)
			.map(|&(h, _, _)| h)
			.collect();
		if !stale.is_empty() {
			for h in &stale {
				self.backend.drop_region(*h);
			}
			self.compiled.retain(|_, &mut (h, _, _)| !stale.contains(&h));
		}
	}

	/// Formation for a dispatcher that installs regions itself (the live
	/// codegen JIT): consume the clock, form regions from the sampled heat
	/// of blocks `covered` does not exclude, and return each with its heat
	/// (sampled retired instructions) per member and in total, hottest seed
	/// first. Heat decays fully between passes; edges persist (bounded).
	pub fn form_now<F: Fn(u64) -> bool>(&mut self, covered: F) -> Vec<(Vec<(u64, u64)>, u64)> {
		self.since_form = 0;
		let nodes: Vec<(u64, u64)> = self
			.heat
			.iter()
			.filter(|&(&pc, _)| !covered(pc))
			.map(|(&pc, &h)| (pc, h))
			.collect();
		let regions = match self.greedy {
			true => {
				let edges: Vec<(u64, u64, u64)> =
					self.edges.iter().map(|(&(a, b), &w)| (a, b, w)).collect();
				form_regions_greedy(&nodes, &edges, self.max_blocks, self.min_heat)
			}
			false => {
				let edges: Vec<(u64, u64)> = self.edges.keys().cloned().collect();
				form_regions(&nodes, &edges, self.max_blocks, self.min_heat)
			}
		};
		let out = regions
			.into_iter()
			.map(|pcs| {
				let members: Vec<(u64, u64)> = pcs
					.iter()
					.map(|&pc| (pc, self.heat.get(&pc).copied().unwrap_or(0)))
					.collect();
				let h = members.iter().map(|&(_, h)| h).sum();
				(members, h)
			})
			.collect();
		self.heat.clear();
		// edges age too: halve every pass, forget the ones that reach zero
		self.edges.retain(|_, w| {
			*w >>= 1;
			*w > 0
		});
		out
	}

	/// Formation pass, driven by the caller once enough has retired. The
	/// caller supplies a way to read a block's ops (from its cache) so the
	/// region emitter sees exactly what the interpreter runs.
	pub fn maybe_form<F>(&mut self, lay: &Layout, current_gen: u32, mut ops_of: F)
	where
		F: FnMut(u64) -> Option<(u64, Vec<BlockOp>)>,
	{
		if self.since_form < self.form_interval {
			return;
		}
		self.since_form = 0;
		let nodes: Vec<(u64, u64)> = self.heat.iter().map(|(&pc, &h)| (pc, h)).collect();
		let regions = match self.greedy {
			true => {
				let edges: Vec<(u64, u64, u64)> =
					self.edges.iter().map(|(&(a, b), &w)| (a, b, w)).collect();
				form_regions_greedy(&nodes, &edges, self.max_blocks, self.min_heat)
			}
			false => {
				let edges: Vec<(u64, u64)> = self.edges.keys().cloned().collect();
				form_regions(&nodes, &edges, self.max_blocks, self.min_heat)
			}
		};
		for region_pcs in regions {
			// Skip only when the region adds NOTHING new. Skipping on ANY
			// already-compiled member froze coverage at the first pass's
			// shape: the hottest block in the guest sat uncompiled forever
			// because every region formed around it also touched a compiled
			// neighbor. Overlap is fine — newer entries overwrite per-pc,
			// and a baked backend's handles are static indexes.
			if region_pcs.iter().all(|pc| {
				self.compiled.contains_key(pc) || self.blacklist.contains(pc)
			}) {
				continue;
			}
			if region_pcs.iter().any(|pc| self.blacklist.contains(pc)) {
				continue;
			}
			let mut blocks = Vec::new();
			let mut ok = true;
			for &pc in &region_pcs {
				match ops_of(pc) {
					Some(b) => blocks.push(b),
					None => {
						ok = false;
						break;
					}
				}
			}
			if !ok {
				// usually a direct-mapped cache slot that has moved on;
				// the code is still hot, so let a later pass retry —
				// unless failures are permanent in this backend's world
				if self.blacklist_on_fail {
					for pc in region_pcs {
						self.blacklist.insert(pc);
					}
				}
				continue;
			}
			let entry_pcs: Vec<u64> = blocks.iter().map(|&(pc, _)| pc).collect();
			// The module is advisory: a verb-style backend compiles it (and
			// refuses an empty one); the record/AOT backends key on the
			// blocks themselves, so emit_region coverage is not a gate.
			let module = emit_region(&blocks, lay).unwrap_or_default();
			match self.backend.compile_src(&blocks, &module, &entry_pcs) {
				Some(handle) => {
					for (idx, &pc) in entry_pcs.iter().enumerate() {
						self.compiled.insert(pc, (handle, idx as u32, current_gen));
					}
				}
				None => {
					if self.blacklist_on_fail {
						for pc in region_pcs {
							self.blacklist.insert(pc);
						}
					}
				}
			}
		}
		// heat decays fully between passes; edges persist (bounded)
		self.heat.clear();
	}
}

/// The AOT bake pipeline's profiling backend: "compiles" every region the
/// dispatcher forms by appending it to a dump file — entry pcs, per-block
/// ops, and the fnv-1a hash of the emitted module (the key the runtime AOT
/// backend will match on). call() is never reached in coverage mode: the
/// run-loop splice counts what WOULD have run compiled and interprets it.
pub struct RecordBackend {
	out: Option<std::io::BufWriter<std::fs::File>>,
	next: u32,
}

impl RecordBackend {
	pub fn new(dump: Option<&std::path::Path>) -> RecordBackend {
		RecordBackend {
			out: dump.and_then(|p| std::fs::File::create(p).ok()).map(std::io::BufWriter::new),
			next: 0,
		}
	}
}

/// The AOT match key: a hash of the region's SOURCE — block pcs and their
/// decoded ops — rather than of the emitted module. It exists so a region
/// emit_region cannot express is still bakeable, and it must stay in exact
/// agreement between the profiling dump and the runtime lookup (both call
/// THIS function; build.rs copies the dump's hash verbatim).
pub fn hash_blocks(blocks: &[(u64, Vec<::cpu::BlockOp>)]) -> u64 {
	let mut h: u64 = 0xcbf29ce484222325;
	let mut mix = |v: u64| {
		for b in v.to_le_bytes() {
			h ^= b as u64;
			h = h.wrapping_mul(0x100000001b3);
		}
	};
	for &(pc, ref ops) in blocks {
		mix(pc);
		mix(ops.len() as u64);
		for op in ops {
			mix(op.imm as u32 as u64);
			mix(op.word as u64);
			mix(op.data as u64 | (op.kind as u64) << 16 | (op.rd as u64) << 24
				| (op.rs1 as u64) << 32 | (op.rs2 as u64) << 40 | (op.len as u64) << 48);
		}
	}
	h
}

pub fn fnv64(bytes: &[u8]) -> u64 {
	let mut h: u64 = 0xcbf29ce484222325;
	for &b in bytes {
		h ^= b as u64;
		h = h.wrapping_mul(0x100000001b3);
	}
	h
}

impl CodegenBackend for RecordBackend {
	fn compile(&mut self, _module: &[u8], _entry_pcs: &[u64]) -> Option<u32> {
		self.next += 1;
		Some(self.next - 1)
	}

	fn compile_src(
		&mut self,
		blocks: &[(u64, Vec<::cpu::BlockOp>)],
		module: &[u8],
		entry_pcs: &[u64],
	) -> Option<u32> {
		if let Some(w) = self.out.as_mut() {
			use std::io::Write;
			let _ = writeln!(w, "REGION {:016x} {}", hash_blocks(blocks), blocks.len());
			for &(pc, ref ops) in blocks {
				let _ = writeln!(w, "B {:x} {}", pc, ops.len());
				for op in ops {
					let _ = writeln!(
						w,
						"O {} {} {} {} {} {} {} {}",
						op.imm, op.word, op.data, op.kind, op.rd, op.rs1, op.rs2, op.len
					);
				}
			}
			let _ = w.flush();
		}
		let _ = entry_pcs;
		self.compile(module, entry_pcs)
	}

	fn call(&mut self, _h: u32, _fuel: u64, _entry: u32) -> u64 {
		0
	}

	fn drop_region(&mut self, _h: u32) {}
}

#[cfg(test)]
mod test_tier2 {
	use super::*;

	/// Mock backend: records compilations, "executes" by returning a fixed
	/// retire count and moving a fake pc — enough to validate dispatch,
	/// caching, staleness and blacklisting without wasm.
	struct Mock {
		compiled: Vec<Vec<u64>>,
		dropped: Vec<u32>,
		fail: bool,
	}
	impl CodegenBackend for Mock {
		fn compile(&mut self, _m: &[u8], entry_pcs: &[u64]) -> Option<u32> {
			if self.fail {
				return None;
			}
			self.compiled.push(entry_pcs.to_vec());
			Some(self.compiled.len() as u32 - 1)
		}
		fn call(&mut self, _h: u32, _fuel: u64, _entry: u32) -> u64 {
			7
		}
		fn drop_region(&mut self, h: u32) {
			self.dropped.push(h);
		}
	}

	fn lay() -> Layout {
		Layout {
			memory64: false, shared: false, max_pages: None, ctx: 2048,
			x_base: 0, f_base: 512, tlb: None, pc_addr: 256, gen_addr: 264,
			baked_gen: 1, guest_dram_base: 0x8000_0000, dram_len: 65536,
			fcsr_addr: 272, res_flag_addr: 280, res_addr_addr: 288,
			ram: Ram::Flat { dram_base: 4096 },
		}
	}

	fn hot_loop(t2: &mut Tier2, a: u64, b: u64, times: u64) {
		for _ in 0..times {
			t2.note_block(a, 20);
			t2.note_block(b, 10);
		}
	}

	fn simple_ops(pc: u64) -> Option<(u64, Vec<BlockOp>)> {
		Some((pc, vec![BlockOp {
			imm: 1, word: 0, data: 0, kind: ::cpu::HOT_ADDI,
			rd: 5, rs1: 5, rs2: 0, len: 4, _pad: 0,
		}]))
	}

	#[test]
	fn forms_compiles_caches_and_invalidates() {
		let mut t2 = Tier2::new(Box::new(Mock { compiled: vec![], dropped: vec![], fail: false }));
		t2.form_interval = 1000;
		t2.min_heat = 100;
		let (a, b) = (0x8000_1000u64, 0x8000_1010u64);
		hot_loop(&mut t2, a, b, 100);
		assert!(t2.lookup(a, 1).is_none(), "nothing compiled yet");
		t2.maybe_form(&lay(), 1, simple_ops);
		let got = t2.lookup(a, 1);
		assert!(got.is_some(), "hot loop compiled");
		assert!(t2.lookup(b, 1).is_some(), "both entries mapped");
		// staleness: a new generation drops the region
		assert!(t2.lookup(a, 2).is_none(), "stale generation invalidates");
		assert!(t2.lookup(b, 2).is_none(), "shared handle fully dropped");
	}

	#[test]
	fn failed_compile_blacklists() {
		let mut t2 = Tier2::new(Box::new(Mock { compiled: vec![], dropped: vec![], fail: true }));
		t2.form_interval = 1000;
		t2.min_heat = 100;
		let (a, b) = (0x8000_2000u64, 0x8000_2010u64);
		hot_loop(&mut t2, a, b, 100);
		t2.maybe_form(&lay(), 1, simple_ops);
		assert!(t2.lookup(a, 1).is_none());
		assert!(t2.blacklist.contains(&a) && t2.blacklist.contains(&b));
		// and it doesn't retry compilation next pass
		hot_loop(&mut t2, a, b, 100);
		t2.maybe_form(&lay(), 1, simple_ops);
		assert!(t2.lookup(a, 1).is_none());
	}
}

// ---- the platform codegen verb -------------------------------------------

/// risc-box patch (codegen feature): the production backend over
/// `enclave:codegen/compiler@0.1.0` (set/codegen.c binds it): `compile`
/// copies a module out of this app's own memory, compiles it with the
/// engine's cranelift, instantiates it over this memory and installs its
/// `run` in this execution view's function table; the returned index IS a
/// function pointer, so a call is an ordinary `call_indirect` — no host
/// crossing, no marshalling.
///
/// The host's limits are per execution view and CUMULATIVE — drop revokes a
/// slot but never returns budget — so this module owns the one policy every
/// machine in the process shares:
///
/// - a module is compiled at most once: identical bytes (the emitter makes
///   modules position-independent, so the same code at another page-aligned
///   address, in another process or another machine, is the same module)
///   reuse the cached table index; a module that failed is never retried;
/// - our budget sits under the host's (modules, attempts, bytes), and the
///   heat a region must show to be compiled doubles every `heat_doubling`
///   compiles, so the budget goes to the hottest code first and is never
///   spent all at once on whatever was warm early;
/// - a missing verb, an exhausted host quota, a binding failure or a run of
///   failed compiles turns compilation off for the life of the process;
///   regions already compiled keep running, everything else interprets.
///
/// Compiled code is never dropped: the host keeps the instance until the
/// store dies anyway, and a kept index is what lets the same code come back
/// (a respawned process, a re-forked machine) without spending budget.
#[cfg(feature = "codegen")]
pub mod verb {
	use std::sync::Mutex;

	/// Host limits per execution view (runtime/component/codegen.rs).
	pub const HOST_MAX_MODULE_BYTES: usize = 256 * 1024;
	pub const HOST_MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;
	pub const HOST_MAX_ATTEMPTS: u32 = 1024;
	pub const HOST_MAX_MODULES: u32 = 256;
	/// set/codegen.c's unwired stub: no host verb behind the import.
	pub const UNAVAILABLE: i64 = -64;

	#[derive(Clone, Debug)]
	pub struct Policy {
		/// Largest module we submit (the host takes 256 KiB).
		pub max_module_bytes: usize,
		/// Modules we may ever compile (successes and failed compiles: the
		/// host reserves a module before compiling and keeps it on failure).
		pub module_budget: u32,
		pub attempt_budget: u32,
		pub byte_budget: u64,
		/// Required heat doubles every this many compiles.
		pub heat_doubling: u32,
		/// Consecutive failed compiles that turn compilation off.
		pub max_consecutive_failures: u32,
	}

	impl Default for Policy {
		fn default() -> Policy {
			Policy {
				max_module_bytes: 128 * 1024,
				// headroom under 256/1024/16 MiB for anything else in the
				// view that might compile (the codegen self-test does)
				module_budget: 240,
				attempt_budget: 960,
				byte_budget: 15 * 1024 * 1024,
				heat_doubling: 48,
				max_consecutive_failures: 6,
			}
		}
	}

	#[derive(Clone, Debug, Default)]
	pub struct Stats {
		pub compiled: u32,
		pub failed: u32,
		pub attempts: u32,
		pub bytes: u64,
		/// Lookups answered from the cache (no compile).
		pub reused: u64,
		pub refused_heat: u64,
		pub refused_budget: u64,
		pub too_large: u64,
		pub compile_us: u64,
		pub max_compile_us: u64,
		pub last_status: i64,
		pub disabled: Option<&'static str>,
	}

	struct State {
		policy: Policy,
		stats: Stats,
		// source key (see super::source_key) -> Some(table index) | None
		// (failed, or too large: either way, never resubmitted)
		cache: ::fnv::FnvHashMap<(u64, u64, u64), Option<u64>>,
		consecutive_failures: u32,
		owner: usize,
	}

	static STATE: Mutex<Option<State>> = Mutex::new(None);

	/// A key for callers that hold module bytes (tests, tools).
	pub fn bytes_key(module: &[u8]) -> (u64, u64, u64) {
		let mut h2: u64 = 0x84222325cbf29ce4;
		for &b in module {
			h2 ^= b as u64;
			h2 = h2.wrapping_mul(0x100000001b3);
		}
		(module.len() as u64, super::fnv64(module), h2)
	}

	/// Arm the verb for this process (idempotent; the first caller's policy
	/// and thread win). Returns whether compilation is still possible.
	pub fn enable(policy: Policy) -> bool {
		let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
		let s = g.get_or_insert_with(|| State {
			policy,
			stats: Stats::default(),
			cache: Default::default(),
			consecutive_failures: 0,
			owner: sys::thread(),
		});
		s.stats.disabled.is_none()
	}

	/// Table indices belong to the execution view (SET thread) that compiled
	/// them: only the enabling thread may call or compile.
	pub fn on_owner_thread() -> bool {
		let g = STATE.lock().unwrap_or_else(|e| e.into_inner());
		match g.as_ref() {
			Some(s) => s.owner == sys::thread(),
			None => false,
		}
	}

	pub fn stats() -> Stats {
		let g = STATE.lock().unwrap_or_else(|e| e.into_inner());
		g.as_ref().map(|s| s.stats.clone()).unwrap_or_default()
	}

	pub fn policy() -> Policy {
		let g = STATE.lock().unwrap_or_else(|e| e.into_inner());
		g.as_ref().map(|s| s.policy.clone()).unwrap_or_default()
	}

	#[derive(Clone, Copy, Debug, PartialEq, Eq)]
	pub enum Got {
		/// These exact bytes were compiled before.
		Cached(u64),
		/// Compiled now (spent budget).
		Compiled(u64),
		/// Not compiled: policy, budget, a past failure, or `may_compile`.
		Refused,
		/// The emitted module exceeds the size cap (remembered as refused).
		TooLarge,
		/// Submitted and refused by the host.
		Failed,
	}

	impl Got {
		pub fn index(self) -> Option<u64> {
			match self {
				Got::Cached(i) | Got::Compiled(i) => Some(i),
				_ => None,
			}
		}
	}

	/// The table index running the module with source key `k`: cached when
	/// it was compiled before; otherwise, when `may_compile`, emitted (only
	/// now: refused regions never pay for emission) and compiled if `heat`
	/// clears `min_heat` scaled by how much budget is already spent and the
	/// budget allows. Anything else: the caller keeps interpreting.
	pub fn lookup<F: FnOnce() -> Option<Vec<u8>>>(
		k: (u64, u64, u64),
		heat: u64,
		min_heat: u64,
		may_compile: bool,
		emit: F,
	) -> Got {
		let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
		let s = match g.as_mut() {
			Some(s) => s,
			None => return Got::Refused,
		};
		if let Some(&hit) = s.cache.get(&k) {
			return match hit {
				Some(i) => {
					s.stats.reused += 1;
					Got::Cached(i)
				}
				None => Got::Refused,
			};
		}
		if s.stats.disabled.is_some() || !may_compile {
			return Got::Refused;
		}
		let p = &s.policy;
		let shift = (s.stats.compiled / p.heat_doubling.max(1)).min(30);
		if heat < min_heat.saturating_mul(1u64 << shift) {
			s.stats.refused_heat += 1;
			return Got::Refused;
		}
		let modules = s.stats.compiled + s.stats.failed;
		if modules >= p.module_budget.min(HOST_MAX_MODULES)
			|| s.stats.attempts >= p.attempt_budget.min(HOST_MAX_ATTEMPTS)
		{
			s.stats.refused_budget += 1;
			return Got::Refused;
		}
		let module = match emit() {
			Some(m) if !m.is_empty() => m,
			_ => return Got::Refused,
		};
		let p = &s.policy;
		if module.len() > p.max_module_bytes.min(HOST_MAX_MODULE_BYTES) {
			s.stats.too_large += 1;
			s.cache.insert(k, None);
			return Got::TooLarge;
		}
		if modules >= p.module_budget.min(HOST_MAX_MODULES)
			|| s.stats.attempts >= p.attempt_budget.min(HOST_MAX_ATTEMPTS)
			|| s.stats.bytes + module.len() as u64 > p.byte_budget.min(HOST_MAX_INPUT_BYTES)
		{
			s.stats.refused_budget += 1;
			return Got::Refused;
		}
		s.stats.attempts += 1;
		s.stats.bytes += module.len() as u64;
		let t0 = std::time::Instant::now();
		let status = sys::compile(&module);
		let us = t0.elapsed().as_micros() as u64;
		s.stats.compile_us += us;
		s.stats.max_compile_us = s.stats.max_compile_us.max(us);
		s.stats.last_status = status;
		if status >= 0 {
			s.stats.compiled += 1;
			s.consecutive_failures = 0;
			s.cache.insert(k, Some(status as u64));
			return Got::Compiled(status as u64);
		}
		s.stats.failed += 1;
		s.consecutive_failures += 1;
		s.cache.insert(k, None);
		s.stats.disabled = match status {
			UNAVAILABLE => Some("verb unavailable"),
			-4 => Some("host binding or table"),
			-5 => Some("host quota"),
			_ if s.consecutive_failures >= s.policy.max_consecutive_failures => {
				Some("repeated compile failures")
			}
			_ => None,
		};
		if let Some(why) = s.stats.disabled {
			eprintln!("[jit] codegen off: {} (status {})", why, status);
		}
		Got::Failed
	}

	/// lookup() for callers holding module bytes that only want the index.
	pub fn get(module: &[u8], heat: u64, min_heat: u64) -> Option<u64> {
		let m = module.to_vec();
		lookup(bytes_key(module), heat, min_heat, true, move || Some(m)).index()
	}

	/// The calling execution view's identity (a SET pthread).
	pub fn thread_id() -> usize {
		sys::thread()
	}

	/// The context block every generated module reads at entry.
	#[repr(C, align(8))]
	pub struct Ctx(std::cell::UnsafeCell<[u64; super::CTX_CELLS]>);
	// Written by the owner thread right before its own calls; generated code
	// reads it. Never shared across execution views in practice.
	unsafe impl Sync for Ctx {}
	pub static CTX: Ctx = Ctx(std::cell::UnsafeCell::new([0; super::CTX_CELLS]));
	impl Ctx {
		pub fn addr(&self) -> u64 {
			self.0.get() as usize as u64
		}
		/// cell = super::CTX_* (a byte offset)
		#[inline(always)]
		pub fn set(&self, cell: u64, v: u64) {
			unsafe {
				let p = (self.0.get() as *mut u64).add((cell / 8) as usize);
				std::ptr::write_volatile(p, v);
			}
		}
	}

	/// Run compiled region `index`: fuel-bounded, entry by block index.
	/// Callers keep the context block current and stay on the owner thread.
	#[inline(always)]
	pub unsafe fn call(index: u64, fuel: u64, entry: u32) -> u64 {
		sys::call(index, fuel, entry)
	}

	#[cfg(target_family = "wasm")]
	mod sys {
		extern "C" {
			fn risc_codegen_compile(bytes: *const u8, len: usize) -> i64;
			fn risc_codegen_thread() -> usize;
		}
		pub fn compile(module: &[u8]) -> i64 {
			unsafe { risc_codegen_compile(module.as_ptr(), module.len()) }
		}
		pub fn thread() -> usize {
			unsafe { risc_codegen_thread() }
		}
		/// A table index is a function pointer: this is a call_indirect of
		/// type (i64, i32) -> i64, the signature the host checked.
		#[inline(always)]
		pub unsafe fn call(index: u64, fuel: u64, entry: u32) -> u64 {
			let f: extern "C" fn(i64, i32) -> i64 = std::mem::transmute(index as usize);
			f(fuel as i64, entry as i32) as u64
		}
	}

	/// Native builds (tests, examples) have no verb: compile reports it
	/// unavailable unless a test installs a stand-in.
	#[cfg(not(target_family = "wasm"))]
	mod sys {
		use std::sync::Mutex;
		pub static TEST_COMPILER: Mutex<Option<fn(&[u8]) -> i64>> = Mutex::new(None);
		pub fn compile(module: &[u8]) -> i64 {
			match *TEST_COMPILER.lock().unwrap() {
				Some(f) => f(module),
				None => super::UNAVAILABLE,
			}
		}
		pub fn thread() -> usize {
			1
		}
		pub unsafe fn call(_index: u64, _fuel: u64, _entry: u32) -> u64 {
			0
		}
	}

	/// Tests: reset the process-wide state and install a stand-in compiler.
	#[cfg(not(target_family = "wasm"))]
	pub fn reset_for_test(compiler: Option<fn(&[u8]) -> i64>, policy: Policy) {
		*sys::TEST_COMPILER.lock().unwrap() = compiler;
		*STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
		enable(policy);
	}

	/// Tests that touch the process-wide verb state take this first.
	#[cfg(test)]
	pub static TEST_SERIAL: Mutex<()> = Mutex::new(());

	#[cfg(test)]
	mod tests {
		use super::*;
		use super::TEST_SERIAL as SERIAL;

		fn ok(_m: &[u8]) -> i64 {
			use std::sync::atomic::{AtomicI64, Ordering};
			static NEXT: AtomicI64 = AtomicI64::new(100);
			NEXT.fetch_add(1, Ordering::Relaxed)
		}
		fn bad(_m: &[u8]) -> i64 {
			-3
		}
		fn quota(_m: &[u8]) -> i64 {
			-5
		}

		#[test]
		fn caches_identical_modules_and_escalates_heat() {
			let _l = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset_for_test(Some(ok), Policy { heat_doubling: 2, ..Policy::default() });
			let a = get(b"module-a", 10, 10).expect("compiles at threshold");
			assert_eq!(get(b"module-a", 0, 10), Some(a), "same bytes reuse, heat irrelevant");
			assert!(get(b"module-b", 10, 10).is_some());
			// two compiles spent: the threshold doubled
			assert!(get(b"module-c", 10, 10).is_none(), "below the doubled threshold");
			assert!(get(b"module-c", 20, 10).is_some());
			let s = stats();
			assert_eq!((s.compiled, s.reused, s.refused_heat), (3, 1, 1));
		}

		#[test]
		fn budget_is_cumulative_and_failures_are_not_retried() {
			let _l = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset_for_test(Some(ok), Policy { module_budget: 2, heat_doubling: 1000, ..Policy::default() });
			assert!(get(b"x1", 1, 1).is_some());
			assert!(get(b"x2", 1, 1).is_some());
			assert!(get(b"x3", 1, 1).is_none(), "module budget spent");
			assert!(get(b"x1", 1, 1).is_some(), "but cached code still runs");
			assert_eq!(stats().refused_budget, 1);

			reset_for_test(Some(bad), Policy { max_consecutive_failures: 3, ..Policy::default() });
			assert!(get(b"y1", 1, 1).is_none());
			assert!(get(b"y1", 1, 1).is_none());
			assert_eq!(stats().attempts, 1, "a failed module is never resubmitted");
			assert!(get(b"y2", 1, 1).is_none());
			assert!(stats().disabled.is_none());
			assert!(get(b"y3", 1, 1).is_none());
			assert_eq!(stats().disabled, Some("repeated compile failures"));
			assert!(get(b"y4", 1, 1).is_none());
			assert_eq!(stats().attempts, 3, "off means off");
		}

		#[test]
		fn missing_verb_and_host_quota_turn_compilation_off() {
			let _l = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset_for_test(None, Policy::default());
			assert!(get(b"z", 1, 1).is_none());
			assert_eq!(stats().disabled, Some("verb unavailable"));
			reset_for_test(Some(quota), Policy::default());
			assert!(get(b"z", 1, 1).is_none());
			assert_eq!(stats().disabled, Some("host quota"));
		}

		#[test]
		fn oversized_modules_are_refused_without_an_attempt() {
			let _l = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset_for_test(Some(ok), Policy { max_module_bytes: 4, ..Policy::default() });
			assert!(get(b"too-large", 1, 1).is_none());
			assert_eq!((stats().attempts, stats().too_large), (0, 1));
		}

		#[test]
		fn refused_regions_are_never_emitted() {
			let _l = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset_for_test(Some(ok), Policy::default());
			let k = (1, 2, 3);
			let r = lookup(k, 5, 10, true, || panic!("emitted below the heat threshold"));
			assert_eq!(r, Got::Refused);
			let r = lookup(k, 50, 10, false, || panic!("emitted while compiles are capped"));
			assert_eq!(r, Got::Refused);
			let r = lookup(k, 50, 10, true, || Some(b"m".to_vec()));
			assert!(matches!(r, Got::Compiled(_)));
			let again = lookup(k, 0, 10, false, || panic!("re-emitted a cached module"));
			assert_eq!(again.index(), r.index());
		}
	}
}
