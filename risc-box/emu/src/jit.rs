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
pub struct Layout {
	pub x_base: u32,          // x[32] as i64
	pub pc_addr: u32,         // u64
	pub gen_addr: u32,        // u32 write-snoop generation cell
	pub baked_gen: u32,       // generation this block was built against
	pub dram_base: u32,       // linear offset of guest DRAM byte 0
	pub guest_dram_base: u64, // 0x8000_0000
	pub dram_len: u64,
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
	/// pc <- const, return const retired
	fn exit(&mut self, pc: u64, retired: u64) {
		self.i32c(0);
		self.i64c(pc as i64);
		self.op(I64_STORE);
		self.memarg(3, self.lay.pc_addr as u64);
		self.i64c(retired as i64);
		self.op(RETURN);
	}
	/// stack: guest address (i64). Leaves LINEAR i32 address on the stack.
	/// Bails (pc=op_addr, return retired) unless the whole `width` fits in
	/// the DRAM window. Uses local 0 as scratch.
	fn dram_addr(&mut self, width: u64, op_addr: u64, retired: u64) {
		// local0 = guest_addr - guest_dram_base
		self.i64c(self.lay.guest_dram_base as i64);
		self.op(0x7d); // i64.sub
		self.op(0x21); // local.set 0
		uleb(&mut self.code, 0);
		// if local0 > dram_len - width (unsigned): bail
		self.op(0x20); // local.get 0
		uleb(&mut self.code, 0);
		self.i64c((self.lay.dram_len - width) as i64);
		self.op(0x56); // i64.gt_u
		self.op(0x04); // if (empty)
		self.op(0x40);
		self.exit(op_addr, retired);
		self.op(0x0b); // end if
		// linear = dram_base + local0 (wrapped to i32)
		self.op(0x20);
		uleb(&mut self.code, 0);
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
		self.exit(next_pc, retired);
		self.op(0x0b);
	}
}

/// Emit ops[..] starting at guest pc `start`. Returns None if any op is
/// outside the supported subset (caller keeps interpreting that block).
pub fn emit_block(ops: &[BlockOp], start: u64, lay: &Layout) -> Option<Vec<u8>> {
	let mut e = Emit { code: Vec::new(), lay };
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
			HOT_ADDI => bin_imm(&mut e, rd, rs1, imm, 0x7c),
			HOT_ADD => bin_reg(&mut e, rd, rs1, rs2, 0x7c),
			HOT_SUB => bin_reg(&mut e, rd, rs1, rs2, 0x7d),
			HOT_AND => bin_reg(&mut e, rd, rs1, rs2, 0x83),
			HOT_OR => bin_reg(&mut e, rd, rs1, rs2, 0x84),
			HOT_XOR => bin_reg(&mut e, rd, rs1, rs2, 0x85),
			HOT_ANDI => bin_imm(&mut e, rd, rs1, imm, 0x83),
			HOT_ORI => bin_imm(&mut e, rd, rs1, imm, 0x84),
			HOT_XORI => bin_imm(&mut e, rd, rs1, imm, 0x85),
			HOT_MUL => bin_reg(&mut e, rd, rs1, rs2, 0x7e),
			HOT_SLL => bin_reg(&mut e, rd, rs1, rs2, 0x86),
			HOT_SRL => bin_reg(&mut e, rd, rs1, rs2, 0x88),
			HOT_SRA => bin_reg(&mut e, rd, rs1, rs2, 0x87),
			HOT_SLLI => shift_imm(&mut e, rd, rs1, op.word, 0x86),
			HOT_SRLI => shift_imm(&mut e, rd, rs1, op.word, 0x88),
			HOT_SRAI => shift_imm(&mut e, rd, rs1, op.word, 0x87),
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
				wrap32(&mut e);
				e.set_x_post(rd);
			}
			HOT_ADDW => {
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.get_x(rs2);
				e.op(0x7c);
				wrap32(&mut e);
				e.set_x_post(rd);
			}
			HOT_SUBW => {
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.get_x(rs2);
				e.op(0x7d);
				wrap32(&mut e);
				e.set_x_post(rd);
			}
			HOT_SLLIW => {
				// body: (x[rs1] << shamt) as i32 as i64, shamt = rs2 field
				e.set_x_pre(rd);
				e.get_x(rs1);
				e.i64c(rs2 as i64);
				e.op(0x86);
				wrap32(&mut e);
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
			HOT_SLLW => w_shift_reg(&mut e, rd, rs1, rs2, 0x74),
			HOT_SRLW => w_shift_reg(&mut e, rd, rs1, rs2, 0x76),
			HOT_SRAW => w_shift_reg(&mut e, rd, rs1, rs2, 0x75),
			HOT_SLT => cmp_reg(&mut e, rd, rs1, rs2, 0x53),
			HOT_SLTU => cmp_reg(&mut e, rd, rs1, rs2, 0x54),
			HOT_SLTI => cmp_imm(&mut e, rd, rs1, imm, 0x53),
			HOT_SLTIU => cmp_imm(&mut e, rd, rs1, imm, 0x54),
			HOT_LD => load(&mut e, rd, rs1, imm, addr, ret_before, 8, I64_LOAD, 3),
			HOT_LW => load(&mut e, rd, rs1, imm, addr, ret_before, 4, 0x34, 2),
			HOT_LWU => load(&mut e, rd, rs1, imm, addr, ret_before, 4, 0x35, 2),
			HOT_LH => load(&mut e, rd, rs1, imm, addr, ret_before, 2, 0x32, 1),
			HOT_LHU => load(&mut e, rd, rs1, imm, addr, ret_before, 2, 0x33, 1),
			HOT_LB => load(&mut e, rd, rs1, imm, addr, ret_before, 1, 0x30, 0),
			HOT_LBU => load(&mut e, rd, rs1, imm, addr, ret_before, 1, 0x31, 0),
			HOT_SD => {
				store(&mut e, rs1, rs2, imm, addr, ret_before, 8, I64_STORE, 3);
				e.gen_check(next, ret_after);
			}
			HOT_SW => {
				store(&mut e, rs1, rs2, imm, addr, ret_before, 4, 0x3e, 2);
				e.gen_check(next, ret_after);
			}
			HOT_SH => {
				store(&mut e, rs1, rs2, imm, addr, ret_before, 2, 0x3d, 1);
				e.gen_check(next, ret_after);
			}
			HOT_SB => {
				store(&mut e, rs1, rs2, imm, addr, ret_before, 1, 0x3c, 0);
				e.gen_check(next, ret_after);
			}
			HOT_BEQ => branch(&mut e, rs1, rs2, 0x51, addr, imm, next, ret_after),
			HOT_BNE => branch(&mut e, rs1, rs2, 0x52, addr, imm, next, ret_after),
			HOT_BLT => branch(&mut e, rs1, rs2, 0x53, addr, imm, next, ret_after),
			HOT_BGE => branch(&mut e, rs1, rs2, 0x59, addr, imm, next, ret_after),
			HOT_BLTU => branch(&mut e, rs1, rs2, 0x54, addr, imm, next, ret_after),
			HOT_BGEU => branch(&mut e, rs1, rs2, 0x5a, addr, imm, next, ret_after),
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
				e.op(0x21); // local.set 0 (target)
				uleb(&mut e.code, 0);
				e.set_x_pre(rd);
				e.i64c(next as i64);
				e.set_x_post(rd);
				e.op(0x20); // local.get 0
				uleb(&mut e.code, 0);
				e.i64c(next as i64);
				e.op(0x52); // i64.ne
				e.op(0x04); // if
				e.op(0x40);
				e.i32c(0);
				e.op(0x20);
				uleb(&mut e.code, 0);
				e.op(I64_STORE);
				e.memarg(3, lay.pc_addr as u64);
				e.i64c(ret_after as i64);
				e.op(RETURN);
				e.op(0x0b); // end if; fall through when target == next
			}
			_ => return None, // outside the subset: keep interpreting
		}
		pc = next;
	}
	// fallthrough exit
	e.exit(pc, ops.len() as u64);
	Some(assemble(e.code))
}

/// ... as i32 as i64 (wrap then sign-extend)
fn wrap32(e: &mut Emit) {
	e.op(0xa7); // i32.wrap_i64
	e.op(0xac); // i64.extend_i32_s
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
	e.dram_addr(width, addr, ret);
	e.op(lop);
	e.memarg(align, 0);
	e.set_x_post(rd);
}

fn store(e: &mut Emit, rs1: u8, rs2: u8, imm: i64, addr: u64, ret: u64, width: u64, sop: u8, align: u8) {
	e.get_x(rs1);
	e.i64c(imm);
	e.op(0x7c);
	e.dram_addr(width, addr, ret);
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
	e.exit(target, ret);
	e.op(0x0b);
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
	// code: one body, one i64 local (scratch)
	let mut body = Vec::new();
	uleb(&mut body, 1);
	uleb(&mut body, 1);
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
