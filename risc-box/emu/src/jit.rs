//! risc-box patch (jit feature): the app-side translator half of
//! PLATFORM-JIT.md — real superblock ops (cpu::BlockOp) emitted as a wasm
//! module. This is the production-shaped successor to
//! examples/jit-proto.rs's synthetic emitter: same hand-encoded binary
//! format, but over the emulator's own IR, with the interpreter-bail
//! contract exec_block already keeps:
//!
//! - every exit leaves pc EXACT: a bail (unsupported op, non-DRAM address,
//!   invalidated code generation after a store) sets pc to the op that did
//!   not run and returns how many ops did; a taken branch/jump sets its
//!   target; falling off the end sets the fallthrough. Exits return
//!   compile-time constants, so the body carries no retired counter and no
//!   per-op pc stores — the dispatcher re-enters wherever pc says.
//! - loads/stores address guest PHYSICAL DRAM through a flat window
//!   (linear = dram_base + (addr - guest_dram_base)); anything outside
//!   (MMIO, faults) bails before any side effect. Address translation is a
//!   later tier (measured separately in jit-proto at 5.6x with inline TLB
//!   probes); this tier is exact for bare/physical addressing and is the
//!   op-semantics core every later tier reuses.
//! - RV64 only: Bit32 guests are refused at emit time.
//!
//! The equivalence tests live in cpu.rs (same module as the private state
//! they compare) and run each generated module under wasmtime — a
//! dev-dependency; nothing here links into the shipped app.

use cpu::BlockOp;
use cpu::*;

/// Where the machine's state lives inside the imported linear memory.
/// In production these are the app's own object addresses; in tests they
/// are a synthetic layout the harness serializes into.
#[derive(Clone)]
pub struct Layout {
	pub x_base: u32,          // x[32] as i64
	pub f_base: u32,          // f[32] as f64 (raw 8-byte cells)
	// Some(_) when the guest runs under paging: memory ops probe the
	// emulator's software TLB (hit -> translated physical -> flat DRAM
	// window; miss/meta-stale -> bail, the interpreter walks and fills so
	// the retry hits). None for bare/physical addressing.
	pub tlb: Option<TlbLayout>,
	pub pc_addr: u32,         // u64
	pub gen_addr: u32,        // u32 write-snoop generation cell
	pub baked_gen: u32,       // generation this block was built against
	pub dram_base: u32,       // linear offset of guest DRAM byte 0
	pub guest_dram_base: u64, // 0x8000_0000
	pub dram_len: u64,
}

/// Where the software TLB's READ and WRITE ways live, mirroring
/// Mmu::translate_address's hit path: set = (vaddr >> 12) & (sets-1);
/// hit iff tags[set] == (vaddr & !0xfff) | 1 && metas[set] == *meta_cache.
#[derive(Clone)]
pub struct TlbLayout {
	pub sets: u32, // power of two (the emulator uses 512)
	pub read_tags: u32,
	pub read_metas: u32,
	pub read_ppns: u32,
	pub write_tags: u32,
	pub write_metas: u32,
	pub write_ppns: u32,
	pub meta_cache: u32, // u32 cell holding tlb_meta_cache
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

struct Emit<'a> {
	code: Vec<u8>,
	lay: &'a Layout,
	// index of the i64 scratch local (0 in single-block mode; shifted in
	// region mode where params occupy the low indices)
	scratch: u32,
	// second i64 scratch (TLB probes need vaddr and the entry offset live
	// at once)
	scratch2: u32,
	// how many `if` labels currently enclose the emission point — a region
	// transfer's br to the dispatch loop must add this to its depth
	if_depth: u32,
	// region mode: map from guest block start pc -> region block index,
	// plus the local indices for the dispatch loop
	region: Option<RegionEmit>,
}

#[derive(Clone)]
struct RegionEmit {
	targets: std::collections::HashMap<u64, u32>,
	cur_local: u32,     // i32: which region block to run next
	retired_local: u32, // i64: ops retired so far
	fuel_param: u32,    // i64 param: stop once retired >= fuel
	// br depth from the CURRENT emission point to the dispatch loop head;
	// set per-block during region emission
	loop_depth: u32,
}

// opcode bytes used below
const I32_CONST: u8 = 0x41;
const I64_CONST: u8 = 0x42;
const I64_LOAD: u8 = 0x29;
const I64_STORE: u8 = 0x37;
const RETURN: u8 = 0x0f;

impl<'a> Emit<'a> {
	fn i32c(&mut self, v: i32) {
		self.code.push(I32_CONST);
		sleb(&mut self.code, v as i64);
	}
	fn i64c(&mut self, v: i64) {
		self.code.push(I64_CONST);
		sleb(&mut self.code, v);
	}
	fn op(&mut self, b: u8) {
		self.code.push(b);
	}
	fn memarg(&mut self, align: u8, off: u64) {
		self.code.push(align);
		uleb(&mut self.code, off);
	}
	/// push x[r]
	fn get_x(&mut self, r: u8) {
		if r == 0 {
			self.i64c(0);
			return;
		}
		self.i32c(0);
		self.op(I64_LOAD);
		self.memarg(3, self.lay.x_base as u64 + r as u64 * 8);
	}
	/// x[rd] <- stack top, via address-first pattern
	fn set_x_pre(&mut self, r: u8) {
		if r != 0 {
			self.i32c(0);
		}
	}
	fn set_x_post(&mut self, r: u8) {
		if r == 0 {
			self.op(0x1a); // drop
			return;
		}
		self.op(I64_STORE);
		self.memarg(3, self.lay.x_base as u64 + r as u64 * 8);
	}
	/// push f[r] bit pattern as i64
	fn get_f_bits(&mut self, r: u8) {
		self.i32c(0);
		self.op(I64_LOAD);
		self.memarg(3, self.lay.f_base as u64 + r as u64 * 8);
	}
	fn set_f_bits_pre(&mut self) {
		self.i32c(0);
	}
	fn set_f_bits_post(&mut self, r: u8) {
		self.op(I64_STORE);
		self.memarg(3, self.lay.f_base as u64 + r as u64 * 8);
	}
	/// push f[r] as f64
	fn get_f(&mut self, r: u8) {
		self.i32c(0);
		self.op(0x2b); // f64.load
		self.memarg(3, self.lay.f_base as u64 + r as u64 * 8);
	}
	fn set_f_post(&mut self, r: u8) {
		self.op(0x39); // f64.store
		self.memarg(3, self.lay.f_base as u64 + r as u64 * 8);
	}

	/// pc <- const, then leave. Single-block mode returns the constant
	/// retired count. Region mode: if pc names another region block, add
	/// this block's retired-so-far and branch back to the dispatch loop;
	/// otherwise return retired_local + the constant.
	fn exit(&mut self, pc: u64, retired: u64) {
		match self.region.clone() {
			None => {
				self.i32c(0);
				self.i64c(pc as i64);
				self.op(I64_STORE);
				self.memarg(3, self.lay.pc_addr as u64);
				self.i64c(retired as i64);
				self.op(RETURN);
			}
			Some(r) => {
				self.op(0x20);
				uleb(&mut self.code, r.retired_local as u64);
				self.i64c(retired as i64);
				self.op(0x7c);
				self.op(0x21);
				uleb(&mut self.code, r.retired_local as u64);
				match r.targets.get(&pc) {
					Some(&idx) => {
						self.i32c(idx as i32);
						self.op(0x21);
						uleb(&mut self.code, r.cur_local as u64);
						self.op(0x0c); // br to dispatch loop
						uleb(&mut self.code, (r.loop_depth + self.if_depth) as u64);
					}
					None => {
						self.i32c(0);
						self.i64c(pc as i64);
						self.op(I64_STORE);
						self.memarg(3, self.lay.pc_addr as u64);
						self.op(0x20);
						uleb(&mut self.code, r.retired_local as u64);
						self.op(RETURN);
					}
				}
			}
		}
	}

	/// A bail: pc <- const, return retired. Bails NEVER transfer within a
	/// region — a bail pc that happens to be a block start must still hand
	/// control back (a gen bail means this module is stale; a memory bail
	/// at a block's first op would otherwise loop forever re-entering it).
	fn bail(&mut self, pc: u64, retired: u64) {
		match self.region.clone() {
			None => self.exit(pc, retired),
			Some(r) => {
				self.i32c(0);
				self.i64c(pc as i64);
				self.op(I64_STORE);
				self.memarg(3, self.lay.pc_addr as u64);
				self.op(0x20);
				uleb(&mut self.code, r.retired_local as u64);
				self.i64c(retired as i64);
				self.op(0x7c);
				self.op(RETURN);
			}
		}
	}

	/// JALR's exit tail after the pc store (target already stored): the
	/// runtime target cannot be an in-region transfer in v1, so it always
	/// returns.
	fn jalr_ret(&mut self, retired: u64) {
		match self.region.clone() {
			None => {
				self.i64c(retired as i64);
				self.op(RETURN);
			}
			Some(r) => {
				self.op(0x20);
				uleb(&mut self.code, r.retired_local as u64);
				self.i64c(retired as i64);
				self.op(0x7c);
				self.op(RETURN);
			}
		}
	}
	/// stack: guest VIRTUAL address (i64). Leaves LINEAR i32 address on the
	/// stack, translating through the software TLB when the layout has one
	/// (bailing on miss) and bounds-checking the DRAM window either way.
	fn dram_addr(&mut self, width: u64, op_addr: u64, retired: u64, write: bool) {
		if self.lay.tlb.is_some() {
			self.tlb_translate(op_addr, retired, write);
		}
		self.dram_addr_flat(width, op_addr, retired);
	}

	/// stack: guest virtual address -> stack: guest PHYSICAL address, or
	/// bail on TLB miss / stale meta. Scratch: vaddr; scratch2: entry off.
	fn tlb_translate(&mut self, op_addr: u64, retired: u64, write: bool) {
		let t = match &self.lay.tlb {
			Some(t) => TlbLayout { ..TlbLayout {
				sets: t.sets,
				read_tags: t.read_tags, read_metas: t.read_metas, read_ppns: t.read_ppns,
				write_tags: t.write_tags, write_metas: t.write_metas, write_ppns: t.write_ppns,
				meta_cache: t.meta_cache,
			} },
			None => unreachable!(),
		};
		let (tags, metas, ppns) = match write {
			false => (t.read_tags, t.read_metas, t.read_ppns),
			true => (t.write_tags, t.write_metas, t.write_ppns),
		};
		let sc = self.scratch as u64;
		let sc2 = self.scratch2 as u64;
		// scratch = vaddr
		self.op(0x21); uleb(&mut self.code, sc);
		// scratch2 = ((vaddr >> 12) & (sets-1)) * 8   (tag/ppn entry offset)
		self.op(0x20); uleb(&mut self.code, sc);
		self.i64c(12);
		self.op(0x88); // shr_u
		self.i64c((t.sets - 1) as i64);
		self.op(0x83); // and
		self.i64c(8);
		self.op(0x7e); // mul
		self.op(0x21); uleb(&mut self.code, sc2);
		// tag hit? mem[tags + scratch2] == (vaddr & !0xfff) | 1
		self.op(0x20); uleb(&mut self.code, sc2);
		self.op(0xa7); // wrap
		self.op(I64_LOAD);
		self.memarg(3, tags as u64);
		self.op(0x20); uleb(&mut self.code, sc);
		self.i64c(!0xfffi64);
		self.op(0x83);
		self.i64c(1);
		self.op(0x84); // or
		self.op(0x52); // i64.ne
		self.op(0x04); self.op(0x40);
		self.bail(op_addr, retired);
		self.op(0x0b);
		// meta fresh? metas is u32-per-set: offset = scratch2/2
		self.op(0x20); uleb(&mut self.code, sc2);
		self.i64c(1);
		self.op(0x88); // shr_u -> *4 scale
		self.op(0xa7);
		self.op(0x28); // i32.load
		self.memarg(2, metas as u64);
		self.i32c(0);
		self.op(0x28);
		self.memarg(2, t.meta_cache as u64);
		self.op(0x47); // i32.ne
		self.op(0x04); self.op(0x40);
		self.bail(op_addr, retired);
		self.op(0x0b);
		// phys = ppns[set] | (vaddr & 0xfff)
		self.op(0x20); uleb(&mut self.code, sc2);
		self.op(0xa7);
		self.op(I64_LOAD);
		self.memarg(3, ppns as u64);
		self.op(0x20); uleb(&mut self.code, sc);
		self.i64c(0xfff);
		self.op(0x83);
		self.op(0x84); // or
	}

	/// stack: guest PHYSICAL address. Leaves LINEAR i32 address on the
	/// stack. Bails unless the whole `width` fits in the DRAM window.
	fn dram_addr_flat(&mut self, width: u64, op_addr: u64, retired: u64) {
		// scratch = guest_addr - guest_dram_base
		self.i64c(self.lay.guest_dram_base as i64);
		self.op(0x7d); // i64.sub
		self.op(0x21); // local.set scratch
		uleb(&mut self.code, self.scratch as u64);
		// if scratch > dram_len - width (unsigned): bail
		self.op(0x20); // local.get scratch
		uleb(&mut self.code, self.scratch as u64);
		self.i64c((self.lay.dram_len - width) as i64);
		self.op(0x56); // i64.gt_u
		self.op(0x04); // if (empty)
		self.op(0x40);
		self.bail(op_addr, retired);
		self.op(0x0b); // end if
		// linear = dram_base + scratch (wrapped to i32)
		self.op(0x20);
		uleb(&mut self.code, self.scratch as u64);
		self.op(0xa7); // i32.wrap_i64
		self.i32c(self.lay.dram_base as i32);
		self.op(0x6a); // i32.add
	}
	/// after a store: if gen cell != baked, exit with pc=next, retired incl.
	fn gen_check(&mut self, next_pc: u64, retired: u64) {
		self.i32c(0);
		self.op(0x28); // i32.load
		self.memarg(2, self.lay.gen_addr as u64);
		self.i32c(self.lay.baked_gen as i32);
		self.op(0x47); // i32.ne
		self.op(0x04);
		self.op(0x40);
		self.bail(next_pc, retired);
		self.op(0x0b);
	}
}

/// Emit ops[..] starting at guest pc `start`. Returns None if any op is
/// outside the supported subset (caller keeps interpreting that block).
pub fn emit_block(ops: &[BlockOp], start: u64, lay: &Layout) -> Option<Vec<u8>> {
	let mut e = Emit { code: Vec::new(), lay, scratch: 0, scratch2: 1, if_depth: 0, region: None };
	if !emit_seq(&mut e, ops, start) {
		return None;
	}
	Some(assemble(e.code))
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
				e.i64c(addr.wrapping_add(imm as u64) as i64);
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
				e.i64c(next as i64);
				e.set_x_post(rd);
				let target = addr.wrapping_add(imm as u64);
				// exec_block exits only when pc != next; a jump to the very
				// next instruction falls through there, so it must here too
				if target != next {
					e.exit(target, ret_after);
				}
			}
			HOT_JALR => {
				// tmp = next; pc = x[rs1] + imm; x[rd] = tmp (order matters
				// when rd == rs1: target uses the OLD rs1). Runtime target:
				// exit only if it differs from next, like exec_block.
				e.get_x(rs1);
				e.i64c(imm);
				e.op(0x7c);
				e.op(0x21); // local.set scratch (target)
				uleb(&mut e.code, e.scratch as u64);
				e.set_x_pre(rd);
				e.i64c(next as i64);
				e.set_x_post(rd);
				e.op(0x20); // local.get scratch
				uleb(&mut e.code, e.scratch as u64);
				e.i64c(next as i64);
				e.op(0x52); // i64.ne
				e.op(0x04); // if
				e.op(0x40);
				e.i32c(0);
				e.op(0x20);
				uleb(&mut e.code, e.scratch as u64);
				e.op(I64_STORE);
				e.memarg(3, e.lay.pc_addr as u64);
				e.jalr_ret(ret_after);
				e.op(0x0b); // end if; fall through when target == next
			}
			HOT_FLD => {
				// f[rd] = f64::from_bits(load_doubleword)
				e.set_f_bits_pre();
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
				e.set_f_bits_pre();
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
			HOT_FSGNJ_D => {
				// f[rd] = (bits(rs2) & SIGN) | (bits(rs1) & !SIGN)
				e.set_f_bits_pre();
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
				e.set_f_bits_pre();
				e.get_x(rs1);
				e.set_f_bits_post(rd);
			}
			HOT_FCVT_D_W => {
				// f[rd] = x[rs1] as i32 as f64 (exact conversion)
				e.set_f_bits_pre();
				e.get_x(rs1);
				e.op(0xa7); // i32.wrap_i64
				e.op(0xb7); // f64.convert_i32_s
				e.set_f_post(rd);
			}
			_ => {
				// outside the subset. A first-op miss means nothing to
				// compile (single-block mode) or a bail stub (region mode);
				// mid-block, emit a bail so the interpreter takes over at
				// exactly this op, and stop emitting.
				if i == 0 && e.region.is_none() {
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

/// Emit a REGION: several blocks in one module, in-region control
/// transfers compiled as branches back through a dispatch loop, exits
/// leaving pc exact like everything else. Signature:
/// run(fuel: i64, entry: i32) -> retired: i64 — the function stops at a
/// block boundary once `retired >= fuel` (device servicing keeps its
/// cadence), and `entry` picks the starting block. A call can retire 0
/// (fuel exhausted at entry, or an unsupported first op): the dispatcher
/// must make progress another way before retrying.
pub fn emit_region(blocks: &[(u64, Vec<BlockOp>)], lay: &Layout) -> Option<Vec<u8>> {
	if blocks.is_empty() || blocks.len() > 512 {
		return None;
	}
	let n = blocks.len();
	let mut targets = std::collections::HashMap::new();
	for (i, &(start, _)) in blocks.iter().enumerate() {
		targets.insert(start, i as u32);
	}
	let mut e = Emit {
		code: Vec::new(),
		lay,
		scratch: 2, // params fuel=0, entry=1; locals scratch=2, cur=3, retired=4, scratch2=5
		scratch2: 5,
		if_depth: 0,
		region: None,
	};
	// cur = entry
	e.op(0x20);
	uleb(&mut e.code, 1);
	e.op(0x21);
	uleb(&mut e.code, 3);
	// dispatch loop
	e.op(0x03); // loop
	e.op(0x40);
	for _ in 0..n {
		e.op(0x02); // block
		e.op(0x40);
	}
	// br_table on cur
	e.op(0x20);
	uleb(&mut e.code, 3);
	e.op(0x0e); // br_table
	uleb(&mut e.code, n as u64);
	for i in 0..n {
		uleb(&mut e.code, i as u64);
	}
	uleb(&mut e.code, 0); // default: block 0
	for (i, &(start, ref ops)) in blocks.iter().enumerate() {
		e.op(0x0b); // end of label B_i; code for block i follows
		// fuel check: retired >= fuel -> pc = start, return retired
		e.op(0x20);
		uleb(&mut e.code, 4);
		e.op(0x20);
		uleb(&mut e.code, 0);
		e.op(0x59); // i64.ge_s
		e.op(0x04); // if
		e.op(0x40);
		e.i32c(0);
		e.i64c(start as i64);
		e.op(I64_STORE);
		e.memarg(3, e.lay.pc_addr as u64);
		e.op(0x20);
		uleb(&mut e.code, 4);
		e.op(RETURN);
		e.op(0x0b);
		// per-op emission with region transfers armed
		e.region = Some(RegionEmit {
			targets: targets.clone(),
			cur_local: 3,
			retired_local: 4,
			fuel_param: 0,
			loop_depth: (n - 1 - i) as u32,
		});
		let ok = emit_seq(&mut e, ops, start);
		debug_assert!(ok);
	}
	e.op(0x0b); // end loop
	e.op(0x00); // unreachable (loop never falls through)
	Some(assemble_region(e.code))
}

/// ... as i32 as i64 (wrap then sign-extend)
fn wrap32(e: &mut Emit) {
	e.op(0xa7); // i32.wrap_i64
	e.op(0xac); // i64.extend_i32_s
}

fn fp_bin(e: &mut Emit, rd: u8, rs1: u8, rs2: u8, fop: u8) {
	e.set_f_bits_pre();
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
	e.op(0x04); // if
	e.op(0x40);
	e.if_depth += 1;
	e.exit(target, ret);
	e.if_depth -= 1;
	e.op(0x0b);
}

fn assemble_region(body_expr: Vec<u8>) -> Vec<u8> {
	let mut m = vec![0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0];
	// type: (i64, i32) -> i64
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	sec.extend_from_slice(&[0x60, 2, 0x7e, 0x7f, 1, 0x7e]);
	m.push(1);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	// import env.mem
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	uleb(&mut sec, 3);
	sec.extend_from_slice(b"env");
	uleb(&mut sec, 3);
	sec.extend_from_slice(b"mem");
	sec.extend_from_slice(&[0x02, 0x00, 1]);
	m.push(2);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	m.extend_from_slice(&[3, 2, 1, 0]);
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	uleb(&mut sec, 3);
	sec.extend_from_slice(b"run");
	sec.extend_from_slice(&[0x00, 0]);
	m.push(7);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	// locals: i64 scratch, i32 cur, i64 retired, i64 scratch2
	let mut body = Vec::new();
	uleb(&mut body, 4);
	uleb(&mut body, 1);
	body.push(0x7e);
	uleb(&mut body, 1);
	body.push(0x7f);
	uleb(&mut body, 1);
	body.push(0x7e);
	uleb(&mut body, 1);
	body.push(0x7e);
	body.extend_from_slice(&body_expr);
	body.push(0x0b);
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	uleb(&mut sec, body.len() as u64);
	sec.extend_from_slice(&body);
	m.push(10);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	m
}

fn assemble(body_expr: Vec<u8>) -> Vec<u8> {
	let mut m = vec![0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0];
	// type: () -> i64
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	sec.extend_from_slice(&[0x60, 0, 1, 0x7e]);
	m.push(1);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	// import env.mem memory {min 1}
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	uleb(&mut sec, 3);
	sec.extend_from_slice(b"env");
	uleb(&mut sec, 3);
	sec.extend_from_slice(b"mem");
	sec.extend_from_slice(&[0x02, 0x00, 1]);
	m.push(2);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	// one func, type 0
	m.extend_from_slice(&[3, 2, 1, 0]);
	// export "run"
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	uleb(&mut sec, 3);
	sec.extend_from_slice(b"run");
	sec.extend_from_slice(&[0x00, 0]);
	m.push(7);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
	// code: one body, two i64 locals (scratch, scratch2)
	let mut body = Vec::new();
	uleb(&mut body, 1);
	uleb(&mut body, 2);
	body.push(0x7e);
	body.extend_from_slice(&body_expr);
	body.push(0x0b); // end
	let mut sec = Vec::new();
	uleb(&mut sec, 1);
	uleb(&mut sec, body.len() as u64);
	sec.extend_from_slice(&body);
	m.push(10);
	uleb(&mut m, sec.len() as u64);
	m.extend_from_slice(&sec);
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
	use std::collections::{HashMap, HashSet};
	let heat: HashMap<u64, u64> = nodes.iter().cloned().collect();
	let mut adj: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();
	for &(a, b, w) in edges {
		adj.entry(a).or_default().push((b, w));
		adj.entry(b).or_default().push((a, w));
	}
	let mut order: Vec<(u64, u64)> = nodes.to_vec();
	order.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
	let mut claimed: HashSet<u64> = HashSet::new();
	let mut out = Vec::new();
	for &(seed, h) in &order {
		if h < min_heat || claimed.contains(&seed) {
			continue;
		}
		let mut members: Vec<u64> = vec![seed];
		let mut inset: HashSet<u64> = members.iter().cloned().collect();
		while members.len() < max_blocks {
			let mut best: Option<(u64, u64)> = None; // (weight, pc)
			for m in &members {
				if let Some(nb) = adj.get(m) {
					for &(pc, w) in nb {
						if !inset.contains(&pc)
							&& !claimed.contains(&pc)
							&& heat.contains_key(&pc)
							&& best.map(|(bw, _)| w > bw).unwrap_or(true)
						{
							best = Some((w, pc));
						}
					}
				}
			}
			match best {
				Some((_, pc)) => {
					members.push(pc);
					inset.insert(pc);
				}
				None => break,
			}
		}
		if members.len() < 2 && !edges.iter().any(|&(a, b, _)| a == seed && b == seed) {
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
	// per-pc heat since the last formation pass (pc -> retired)
	heat: std::collections::HashMap<u64, u64>,
	// observed block successions for formation
	edges: std::collections::HashMap<(u64, u64), u64>,
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
			heat: std::collections::HashMap::new(),
			edges: std::collections::HashMap::new(),
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
			x_base: 0, f_base: 512, tlb: None, pc_addr: 256, gen_addr: 264,
			baked_gen: 1, dram_base: 4096, guest_dram_base: 0x8000_0000,
			dram_len: 65536,
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
