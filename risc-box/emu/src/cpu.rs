extern crate fnv;

// risc-box patch: FnvHashMap import removed (DecodeCache is direct-mapped now)

use mmu::{AddressingMode, Mmu};
use terminal::Terminal;

// risc-box patch: log the first few undecodable instruction words (PC + word)
// so a missing-opcode SIGILL in a guest program is diagnosable. Rate-limited;
// harmless in production (a well-formed guest never trips it).
fn log_illegal(pc: u64, word: u32) {
	use std::sync::atomic::{AtomicU32, Ordering};
	static N: AtomicU32 = AtomicU32::new(0);
	if N.fetch_add(1, Ordering::Relaxed) < 16 {
		eprintln!("[emu] illegal instruction pc={:#x} word={:#010x}", pc, word);
	}
}

const CSR_CAPACITY: usize = 4096;

const CSR_USTATUS_ADDRESS: u16 = 0x000;
const CSR_FFLAGS_ADDRESS: u16 = 0x001;
const CSR_FRM_ADDRESS: u16 = 0x002;
const CSR_FCSR_ADDRESS: u16 = 0x003;
const CSR_UIE_ADDRESS: u16 = 0x004;
const CSR_UTVEC_ADDRESS: u16 = 0x005;
const _CSR_USCRATCH_ADDRESS: u16 = 0x040;
const CSR_UEPC_ADDRESS: u16 = 0x041;
const CSR_UCAUSE_ADDRESS: u16 = 0x042;
const CSR_UTVAL_ADDRESS: u16 = 0x043;
const _CSR_UIP_ADDRESS: u16 = 0x044;
const CSR_SSTATUS_ADDRESS: u16 = 0x100;
const CSR_SEDELEG_ADDRESS: u16 = 0x102;
const CSR_SIDELEG_ADDRESS: u16 = 0x103;
const CSR_SIE_ADDRESS: u16 = 0x104;
const CSR_STVEC_ADDRESS: u16 = 0x105;
const _CSR_SSCRATCH_ADDRESS: u16 = 0x140;
const CSR_SEPC_ADDRESS: u16 = 0x141;
const CSR_SCAUSE_ADDRESS: u16 = 0x142;
const CSR_STVAL_ADDRESS: u16 = 0x143;
const CSR_SIP_ADDRESS: u16 = 0x144;
const CSR_SATP_ADDRESS: u16 = 0x180;
const CSR_MSTATUS_ADDRESS: u16 = 0x300;
const CSR_MISA_ADDRESS: u16 = 0x301;
const CSR_MEDELEG_ADDRESS: u16 = 0x302;
const CSR_MIDELEG_ADDRESS: u16 = 0x303;
const CSR_MIE_ADDRESS: u16 = 0x304;

const CSR_MTVEC_ADDRESS: u16 = 0x305;
const _CSR_MSCRATCH_ADDRESS: u16 = 0x340;
const CSR_MEPC_ADDRESS: u16 = 0x341;
const CSR_MCAUSE_ADDRESS: u16 = 0x342;
const CSR_MTVAL_ADDRESS: u16 = 0x343;
const CSR_MIP_ADDRESS: u16 = 0x344;
const _CSR_PMPCFG0_ADDRESS: u16 = 0x3a0;
const _CSR_PMPADDR0_ADDRESS: u16 = 0x3b0;
const _CSR_MCYCLE_ADDRESS: u16 = 0xb00;
const CSR_CYCLE_ADDRESS: u16 = 0xc00;
const CSR_TIME_ADDRESS: u16 = 0xc01;
const _CSR_INSERT_ADDRESS: u16 = 0xc02;
const _CSR_MHARTID_ADDRESS: u16 = 0xf14;

// risc-box patch: retired instructions between device services (see Cpu::tick).
// The devices this machine has are a timer, a UART and three virtio queues;
// none of them need to be looked at 13 million times a second, and looking
// cost more than the instructions did. 64 keeps timer granularity far finer
// than the guest's 100 Hz tick while removing 63/64 of the overhead.
const DEVICE_TICK_INTERVAL: u64 = 32;

const MIP_MEIP: u64 = 0x800;
pub const MIP_MTIP: u64 = 0x080;
pub const MIP_MSIP: u64 = 0x008;
pub const MIP_SEIP: u64 = 0x200;
const MIP_STIP: u64 = 0x020;
const MIP_SSIP: u64 = 0x002;

// risc-box patch: superblock ("trace") cache. A block is a run of
// predecoded instructions starting at a virtual pc, built lazily on first
// execution and executed from a single probe: one tag+meta compare covers
// every instruction in the run, where the per-instruction cache paid a
// probe per retired instruction. Invariants that keep this exactly
// equivalent to single-stepping:
// - a block holds hot-set ops (kind != 0) with at most ONE non-hot op,
//   always LAST — so an instruction that can arm the interrupt check,
//   change translation state, or park the hart ends its block, and the
//   run loop observes the effect at the same boundary single-stepping
//   would;
// - every op lives in the block's first (only) page, fill-eligible like
//   the old per-instruction cache (offset <= 0xff8), and the page is
//   marked for the SMC write snoop; hot stores re-check the meta after
//   executing so a block writing over ITSELF stops before running stale
//   ops (the write-then-execute gate in bench.py);
// - a trap or taken branch exits the block with pc exact; JAL/JALR are
//   terminal at build time so slots aren't wasted on unreachable tails.
#[derive(Clone, Copy)]
pub(crate) struct BlockOp {
	pub(crate) imm: i32,
	pub(crate) word: u32,
	pub(crate) data: u16, // INSTRUCTIONS index | ICACHE_LEN4
	pub(crate) kind: u8,
	pub(crate) rd: u8,
	pub(crate) rs1: u8,
	pub(crate) rs2: u8,
	pub(crate) len: u8, // 2 or 4
	pub(crate) _pad: u8
}

impl BlockOp {
	pub(crate) const EMPTY: BlockOp = BlockOp {
		imm: 0, word: 0, data: 0, kind: 0, rd: 0, rs1: 0, rs2: 0, len: 0, _pad: 0
	};
}

// risc-box patch: a block is tagged by the PHYSICAL page it was decoded
// from plus the write-snoop code generation — the two things its cached
// content actually depends on. It is deliberately NOT tagged by
// translation state: satp writes and SFENCE.VMA flush the TLB on every
// context switch, and when the meta embedded the TLB generation every
// switch threw away every block in the machine — page-fault storms
// (desktop boot, app launch) spent their time rebuilding blocks instead
// of running them. The probe instead re-translates the start pc through
// the TLB (a hit is a few compares; a miss re-walks exactly as a fetch
// would) and compares the physical page, so a remapped pc can never run
// a stale block while an unchanged mapping keeps its blocks across
// flushes.
#[derive(Clone, Copy)]
struct BlockHead {
	tag: u64, // start pc (0 = never valid: DRAM starts at 0x80000000)
	phys_page: u64, // physical page the ops were decoded from
	count: u32,
	code_gen: u32 // mmu.code_gen() at build time
}

impl BlockHead {
	const EMPTY: BlockHead = BlockHead { tag: 0, phys_page: 0, count: 0, code_gen: 0 };
}

const BLOCK_SLOTS: usize = 0x8000; // direct-mapped by (pc >> 1); 32k x (24B + 32x16B) = 17 MiB
const BLOCK_MAX: usize = 32; // ops per block

// risc-box patch: hot-op ids. SB/SH/SW/SD are 1..=4 so "is this a store"
// — the ops that need the in-block SMC meta re-check — is one range
// compare. The set is the integer
// instructions that dominate any Linux dynamic mix; everything else keeps
// the INSTRUCTIONS-table path. Each exec_hot arm is a verbatim copy of the
// table closure with the parse_format_* call replaced by the entry fields.
pub(crate) const HOT_ADDI: u8 = 7;
pub(crate) const HOT_ADD: u8 = 8;
pub(crate) const HOT_LD: u8 = 9;
pub(crate) const HOT_SD: u8 = 4;
pub(crate) const HOT_LW: u8 = 10;
pub(crate) const HOT_SW: u8 = 3;
pub(crate) const HOT_BEQ: u8 = 11;
pub(crate) const HOT_BNE: u8 = 12;
pub(crate) const HOT_BLT: u8 = 13;
pub(crate) const HOT_BGE: u8 = 14;
pub(crate) const HOT_BLTU: u8 = 15;
pub(crate) const HOT_BGEU: u8 = 16;
pub(crate) const HOT_LUI: u8 = 17;
pub(crate) const HOT_AUIPC: u8 = 18;
pub(crate) const HOT_JAL: u8 = 19;
pub(crate) const HOT_JALR: u8 = 20;
pub(crate) const HOT_ANDI: u8 = 21;
pub(crate) const HOT_ORI: u8 = 22;
pub(crate) const HOT_XORI: u8 = 23;
pub(crate) const HOT_AND: u8 = 24;
pub(crate) const HOT_OR: u8 = 25;
pub(crate) const HOT_XOR: u8 = 26;
pub(crate) const HOT_SUB: u8 = 27;
pub(crate) const HOT_SLLI: u8 = 28;
pub(crate) const HOT_SRLI: u8 = 29;
pub(crate) const HOT_SRAI: u8 = 30;
pub(crate) const HOT_ADDIW: u8 = 31;
pub(crate) const HOT_ADDW: u8 = 32;
pub(crate) const HOT_SUBW: u8 = 33;
pub(crate) const HOT_SLLIW: u8 = 34;
pub(crate) const HOT_SRLIW: u8 = 35;
pub(crate) const HOT_SRAIW: u8 = 36;
pub(crate) const HOT_SLLW: u8 = 37;
pub(crate) const HOT_SRLW: u8 = 38;
pub(crate) const HOT_SRAW: u8 = 39;
pub(crate) const HOT_SLL: u8 = 40;
pub(crate) const HOT_SRL: u8 = 41;
pub(crate) const HOT_SRA: u8 = 42;
pub(crate) const HOT_SLT: u8 = 43;
pub(crate) const HOT_SLTI: u8 = 44;
pub(crate) const HOT_SLTU: u8 = 45;
pub(crate) const HOT_SLTIU: u8 = 46;
pub(crate) const HOT_MUL: u8 = 47;
pub(crate) const HOT_LB: u8 = 48;
pub(crate) const HOT_LBU: u8 = 49;
pub(crate) const HOT_LH: u8 = 50;
pub(crate) const HOT_LHU: u8 = 51;
pub(crate) const HOT_LWU: u8 = 52;
pub(crate) const HOT_FSW: u8 = 5;
pub(crate) const HOT_FSD: u8 = 6;
// stores are 1..=HOT_STORE_MAX so the in-block SMC re-check is one compare
pub(crate) const HOT_STORE_MAX: u8 = 6;
pub(crate) const HOT_FLD: u8 = 53;
pub(crate) const HOT_FLW: u8 = 54;
pub(crate) const HOT_FADD_D: u8 = 55;
pub(crate) const HOT_FSUB_D: u8 = 56;
pub(crate) const HOT_FMUL_D: u8 = 57;
pub(crate) const HOT_FDIV_D: u8 = 58;
pub(crate) const HOT_FSGNJ_D: u8 = 59;
pub(crate) const HOT_FMV_X_D: u8 = 60;
pub(crate) const HOT_FMV_D_X: u8 = 61;
pub(crate) const HOT_FCVT_D_W: u8 = 62;
pub(crate) const HOT_SB: u8 = 1;
pub(crate) const HOT_SH: u8 = 2;

// risc-box patch: fill-time classification for the predecode cache. Keyed
// by the NAME of the INSTRUCTIONS entry the decode already matched — the
// hot path can never disagree with the table about which instruction a
// word is, because the kind is derived from the table's own match. Returns
// (kind, rd, rs1, rs2, imm); kind 0 means "not in the hot set". The stored
// immediates all fit in i32 (I/S/B: 12-13 bits, U: the sign-extended
// upper-immediate value itself, J: 21 bits); shift amounts are re-read
// from the word at execution so the xlen-dependent masking in the table
// bodies stays exactly where it was.
fn classify_hot(name: &str, word: u32) -> (u8, u8, u8, u8, i32) {
	let kind = match name {
		"ADDI" => HOT_ADDI,
		"ADD" => HOT_ADD,
		"LD" => HOT_LD,
		"SD" => HOT_SD,
		"LW" => HOT_LW,
		"SW" => HOT_SW,
		"BEQ" => HOT_BEQ,
		"BNE" => HOT_BNE,
		"BLT" => HOT_BLT,
		"BGE" => HOT_BGE,
		"BLTU" => HOT_BLTU,
		"BGEU" => HOT_BGEU,
		"LUI" => HOT_LUI,
		"AUIPC" => HOT_AUIPC,
		"JAL" => HOT_JAL,
		"JALR" => HOT_JALR,
		"ANDI" => HOT_ANDI,
		"ORI" => HOT_ORI,
		"XORI" => HOT_XORI,
		"AND" => HOT_AND,
		"OR" => HOT_OR,
		"XOR" => HOT_XOR,
		"SUB" => HOT_SUB,
		"SLLI" => HOT_SLLI,
		"SRLI" => HOT_SRLI,
		"SRAI" => HOT_SRAI,
		"ADDIW" => HOT_ADDIW,
		"ADDW" => HOT_ADDW,
		"SUBW" => HOT_SUBW,
		"SLLIW" => HOT_SLLIW,
		"SRLIW" => HOT_SRLIW,
		"SRAIW" => HOT_SRAIW,
		"SLLW" => HOT_SLLW,
		"SRLW" => HOT_SRLW,
		"SRAW" => HOT_SRAW,
		"SLL" => HOT_SLL,
		"SRL" => HOT_SRL,
		"SRA" => HOT_SRA,
		"SLT" => HOT_SLT,
		"SLTI" => HOT_SLTI,
		"SLTU" => HOT_SLTU,
		"SLTIU" => HOT_SLTIU,
		"MUL" => HOT_MUL,
		"LB" => HOT_LB,
		"LBU" => HOT_LBU,
		"LH" => HOT_LH,
		"LHU" => HOT_LHU,
		"LWU" => HOT_LWU,
		"SB" => HOT_SB,
		"SH" => HOT_SH,
		"FSW" => HOT_FSW,
		"FSD" => HOT_FSD,
		"FLD" => HOT_FLD,
		"FLW" => HOT_FLW,
		"FADD.D" => HOT_FADD_D,
		"FSUB.D" => HOT_FSUB_D,
		"FMUL.D" => HOT_FMUL_D,
		"FDIV.D" => HOT_FDIV_D,
		"FSGNJ.D" => HOT_FSGNJ_D,
		"FMV.X.D" => HOT_FMV_X_D,
		"FMV.D.X" => HOT_FMV_D_X,
		"FCVT.D.W" => HOT_FCVT_D_W,
		_ => 0
	};
	let rd = ((word >> 7) & 0x1f) as u8;
	let rs1 = ((word >> 15) & 0x1f) as u8;
	let rs2 = ((word >> 20) & 0x1f) as u8;
	// The immediate each hot arm expects, by the format its table closure
	// parsed (parse_format_i/s/b/u/j reproduced bit for bit).
	let imm: i32 = match kind {
		HOT_ADDI | HOT_SLTI | HOT_SLTIU | HOT_XORI | HOT_ORI | HOT_ANDI
		| HOT_ADDIW | HOT_JALR | HOT_LB | HOT_LBU | HOT_LH | HOT_LHU
		| HOT_LW | HOT_LWU | HOT_LD | HOT_FLD | HOT_FLW => (
			match word & 0x80000000 {
				0x80000000 => 0xfffff800u32,
				_ => 0
			} | ((word >> 20) & 0x000007ff)
		) as i32,
		HOT_SB | HOT_SH | HOT_SW | HOT_SD | HOT_FSW | HOT_FSD => (
			match word & 0x80000000 {
				0x80000000 => 0xfffff000u32,
				_ => 0
			} | ((word >> 20) & 0xfe0) | ((word >> 7) & 0x1f)
		) as i32,
		HOT_BEQ | HOT_BNE | HOT_BLT | HOT_BGE | HOT_BLTU | HOT_BGEU => (
			match word & 0x80000000 {
				0x80000000 => 0xfffff000u32,
				_ => 0
			} | ((word << 4) & 0x00000800)
				| ((word >> 20) & 0x000007e0)
				| ((word >> 7) & 0x0000001e)
		) as i32,
		HOT_LUI | HOT_AUIPC => (word & 0xfffff000) as i32,
		HOT_JAL => (
			match word & 0x80000000 {
				0x80000000 => 0xfff00000u32,
				_ => 0
			} | (word & 0x000ff000)
				| ((word & 0x00100000) >> 9)
				| ((word & 0x7fe00000) >> 20)
		) as i32,
		_ => 0
	};
	(kind, rd, rs1, rs2, imm)
}

/// Emulates a RISC-V CPU core
pub struct Cpu {
	clock: u64,
	xlen: Xlen,
	privilege_mode: PrivilegeMode,
	wfi: bool,
	// using only lower 32bits of x, pc, and csr registers
	// for 32-bit mode
	x: [i64; 32],
	f: [f64; 32],
	pc: u64,
	csr: [u64; CSR_CAPACITY],
	mmu: Mmu,
	reservation: u64, // @TODO: Should support multiple address reservations
	is_reservation_set: bool,
	_dump_flag: bool,
	decode_cache: DecodeCache,
	// risc-box patch: superblock cache (see BlockOp/BlockHead above).
	// heads[slot] tags a run of ops[slot*BLOCK_MAX ..][..count].
	block_heads: Vec<BlockHead>,
	block_ops: Vec<BlockOp>,
	// risc-box patch (tier2 feature): the region dispatcher in coverage
	// mode — forms regions from live heat/edges, "compiles" them into a
	// recording backend, and the run loop below counts the retired
	// instructions that would have executed as compiled code.
	#[cfg(feature = "tier2")]
	tier2: Option<Box<Tier2State>>,
	// risc-box patch (blockstats feature): per-slot execution/retired
	// counters plus a histogram of retired-instructions bucketed by how many
	// times the retiring block had executed when replaced — the coverage
	// number PLATFORM-JIT.md needs (how much of the dynamic mix a region
	// JIT could compile).
	#[cfg(feature = "blockstats")]
	stat_execs: Vec<u64>,
	#[cfg(feature = "blockstats")]
	stat_retired: Vec<u64>,
	#[cfg(feature = "blockstats")]
	stat_hist: [u64; 6], // buckets by exec count: 1-3,4-15,16-63,64-255,256-4095,4096+
	#[cfg(feature = "blockstats")]
	stat_singlestep: u64,
	// block-graph edges (pred pc -> succ pc -> count) and per-pc node
	// counts (execs, retired), for region/loop discovery at dump time
	#[cfg(feature = "blockstats")]
	stat_edges: std::collections::HashMap<(u64, u64), u64>,
	#[cfg(feature = "blockstats")]
	stat_nodes: std::collections::HashMap<u64, (u64, u64)>,
	#[cfg(feature = "blockstats")]
	stat_prev: u64,
	unsigned_data_mask: u64,
	// risc-box patch: instructions retired since the last device service
	// (blocks may overshoot a boundary by up to BLOCK_MAX-1; the true count
	// is what device clocks advance by), and whether an interrupt check is
	// owed before the next instruction (see run).
	since_service: u64,
	check_interrupt: bool
}

#[derive(Clone)]
pub enum Xlen {
	Bit32,
	Bit64
	// @TODO: Support Bit128
}

#[derive(Clone)]
#[allow(dead_code)]
pub enum PrivilegeMode {
	User,
	Supervisor,
	Reserved,
	Machine
}

pub struct Trap {
	pub trap_type: TrapType,
	pub value: u64 // Trap type specific value
}

#[allow(dead_code)]
pub enum TrapType {
	InstructionAddressMisaligned,
	InstructionAccessFault,
	IllegalInstruction,
	Breakpoint,
	LoadAddressMisaligned,
	LoadAccessFault,
	StoreAddressMisaligned,
	StoreAccessFault,
	EnvironmentCallFromUMode,
	EnvironmentCallFromSMode,
	EnvironmentCallFromMMode,
	InstructionPageFault,
	LoadPageFault,
	StorePageFault,
	UserSoftwareInterrupt,
	SupervisorSoftwareInterrupt,
	MachineSoftwareInterrupt,
	UserTimerInterrupt,
	SupervisorTimerInterrupt,
	MachineTimerInterrupt,
	UserExternalInterrupt,
	SupervisorExternalInterrupt,
	MachineExternalInterrupt
}

fn _get_privilege_mode_name(mode: &PrivilegeMode) -> &'static str {
	match mode {
		PrivilegeMode::User => "User",
		PrivilegeMode::Supervisor => "Supervisor",
		PrivilegeMode::Reserved => "Reserved",
		PrivilegeMode::Machine => "Machine"
	}
}

// bigger number is higher privilege level
fn get_privilege_encoding(mode: &PrivilegeMode) -> u8 {
	match mode {
		PrivilegeMode::User => 0,
		PrivilegeMode::Supervisor => 1,
		PrivilegeMode::Reserved => panic!(),
		PrivilegeMode::Machine => 3
	}
}

/// Returns `PrivilegeMode` from encoded privilege mode bits
pub fn get_privilege_mode(encoding: u64) -> PrivilegeMode {
	match encoding {
		0 => PrivilegeMode::User,
		1 => PrivilegeMode::Supervisor,
		3 => PrivilegeMode::Machine,
		_ => panic!("Unknown privilege uncoding")
	}
}

fn _get_trap_type_name(trap_type: &TrapType) -> &'static str {
	match trap_type {
		TrapType::InstructionAddressMisaligned => "InstructionAddressMisaligned",
		TrapType::InstructionAccessFault => "InstructionAccessFault",
		TrapType::IllegalInstruction => "IllegalInstruction",
		TrapType::Breakpoint => "Breakpoint",
		TrapType::LoadAddressMisaligned => "LoadAddressMisaligned",
		TrapType::LoadAccessFault => "LoadAccessFault",
		TrapType::StoreAddressMisaligned => "StoreAddressMisaligned",
		TrapType::StoreAccessFault => "StoreAccessFault",
		TrapType::EnvironmentCallFromUMode => "EnvironmentCallFromUMode",
		TrapType::EnvironmentCallFromSMode => "EnvironmentCallFromSMode",
		TrapType::EnvironmentCallFromMMode => "EnvironmentCallFromMMode",
		TrapType::InstructionPageFault => "InstructionPageFault",
		TrapType::LoadPageFault => "LoadPageFault",
		TrapType::StorePageFault => "StorePageFault",
		TrapType::UserSoftwareInterrupt => "UserSoftwareInterrupt",
		TrapType::SupervisorSoftwareInterrupt => "SupervisorSoftwareInterrupt",
		TrapType::MachineSoftwareInterrupt => "MachineSoftwareInterrupt",
		TrapType::UserTimerInterrupt => "UserTimerInterrupt",
		TrapType::SupervisorTimerInterrupt => "SupervisorTimerInterrupt",
		TrapType::MachineTimerInterrupt => "MachineTimerInterrupt",
		TrapType::UserExternalInterrupt => "UserExternalInterrupt",
		TrapType::SupervisorExternalInterrupt => "SupervisorExternalInterrupt",
		TrapType::MachineExternalInterrupt => "MachineExternalInterrupt"
	}
}

fn get_trap_cause(trap: &Trap, xlen: &Xlen) -> u64 {
	let interrupt_bit = match xlen {
		Xlen::Bit32 => 0x80000000 as u64,
		Xlen::Bit64 => 0x8000000000000000 as u64,
	};
	match trap.trap_type {
		TrapType::InstructionAddressMisaligned => 0,
		TrapType::InstructionAccessFault => 1,
		TrapType::IllegalInstruction => 2,
		TrapType::Breakpoint => 3,
		TrapType::LoadAddressMisaligned => 4,
		TrapType::LoadAccessFault => 5,
		TrapType::StoreAddressMisaligned => 6,
		TrapType::StoreAccessFault => 7,
		TrapType::EnvironmentCallFromUMode => 8,
		TrapType::EnvironmentCallFromSMode => 9,
		TrapType::EnvironmentCallFromMMode => 11,
		TrapType::InstructionPageFault => 12,
		TrapType::LoadPageFault => 13,
		TrapType::StorePageFault => 15,
		TrapType::UserSoftwareInterrupt => interrupt_bit,
		TrapType::SupervisorSoftwareInterrupt => interrupt_bit + 1,
		TrapType::MachineSoftwareInterrupt => interrupt_bit + 3,
		TrapType::UserTimerInterrupt => interrupt_bit + 4,
		TrapType::SupervisorTimerInterrupt => interrupt_bit + 5,
		TrapType::MachineTimerInterrupt => interrupt_bit + 7,
		TrapType::UserExternalInterrupt => interrupt_bit + 8,
		TrapType::SupervisorExternalInterrupt => interrupt_bit + 9,
		TrapType::MachineExternalInterrupt => interrupt_bit + 11
	}
}

#[cfg(feature = "aot")]
include!(concat!(env!("OUT_DIR"), "/aot_regions.rs"));

/// risc-box patch (aot): the baked-region backend. compile() is a hash
/// lookup into the tables build.rs generated; the run-loop splice calls the
/// baked function directly by handle, so call() here is never reached.
#[cfg(feature = "aot")]
pub struct AotBackend;

#[cfg(feature = "aot")]
impl ::jit::CodegenBackend for AotBackend {
	fn compile(&mut self, module: &[u8], _entry_pcs: &[u64]) -> Option<u32> {
		let h = ::jit::fnv64(module);
		AOT_HASHES.iter().position(|&x| x == h).map(|i| i as u32)
	}
	fn call(&mut self, _h: u32, _fuel: u64, _entry: u32) -> u64 {
		0
	}
	fn drop_region(&mut self, _h: u32) {}
}

#[cfg(feature = "tier2")]
pub struct Tier2State {
	pub t2: ::jit::Tier2,
	pub lay: ::jit::Layout,
	/// retired in blocks whose entry pc had a live compiled region
	pub covered: u64,
	/// all retired in block dispatches while tier2 was enabled
	pub total: u64,
	/// 1-in-8 service-interval recording window (see note_retire): heat and
	/// edges are sampled, the formation clock and coverage counters are not
	pub window: bool,
	pub passes: u64,
}

impl Cpu {
	/// Creates a new `Cpu`.
	///
	/// # Arguments
	/// * `Terminal`
	pub fn new(terminal: Box<dyn Terminal>) -> Self {
		let mut cpu = Cpu {
			clock: 0,
			xlen: Xlen::Bit64,
			privilege_mode: PrivilegeMode::Machine,
			wfi: false,
			x: [0; 32],
			f: [0.0; 32],
			pc: 0,
			csr: [0; CSR_CAPACITY],
			mmu: Mmu::new(Xlen::Bit64, terminal),
			reservation: 0,
			is_reservation_set: false,
			_dump_flag: false,
			decode_cache: DecodeCache::new(),
			// risc-box patch: block cache starts empty (tag 0 = invalid)
			block_heads: vec![BlockHead::EMPTY; BLOCK_SLOTS],
			#[cfg(feature = "tier2")]
			tier2: None,
			block_ops: vec![BlockOp::EMPTY; BLOCK_SLOTS * BLOCK_MAX],
			#[cfg(feature = "blockstats")]
			stat_execs: vec![0; BLOCK_SLOTS],
			#[cfg(feature = "blockstats")]
			stat_retired: vec![0; BLOCK_SLOTS],
			#[cfg(feature = "blockstats")]
			stat_hist: [0; 6],
			#[cfg(feature = "blockstats")]
			stat_singlestep: 0,
			#[cfg(feature = "blockstats")]
			stat_edges: std::collections::HashMap::new(),
			#[cfg(feature = "blockstats")]
			stat_nodes: std::collections::HashMap::new(),
			#[cfg(feature = "blockstats")]
			stat_prev: 0,
			unsigned_data_mask: 0xffffffffffffffff,
			// risc-box patch: service devices after the first instruction, so
			// a machine that traps immediately still sees its clint before
			// running far.
			since_service: DEVICE_TICK_INTERVAL - 1,
			check_interrupt: true
		};
		cpu.x[0xb] = 0x1020; // I don't know why but Linux boot seems to require this initialization
		cpu.write_csr_raw(CSR_MISA_ADDRESS, 0x800000008014312f);
		cpu
	}

	/// Updates Program Counter content
	///
	/// # Arguments
	/// * `value`
	pub fn update_pc(&mut self, value: u64) {
		self.pc = value;
	}

	/// Updates XLEN, 32-bit or 64-bit
	///
	/// # Arguments
	/// * `xlen`
	pub fn update_xlen(&mut self, xlen: Xlen) {
		self.xlen = xlen.clone();
		self.unsigned_data_mask = match xlen {
			Xlen::Bit32 => 0xffffffff,
			Xlen::Bit64 => 0xffffffffffffffff
		};
		self.mmu.update_xlen(xlen.clone());
	}

	/// Reads integer register content
	///
	/// # Arguments
	/// * `reg` Register number. Must be 0-31
	pub fn read_register(&self, reg: u8) -> i64 {
		debug_assert!(reg <= 31, "reg must be 0-31. {}", reg);
		match reg {
			0 => 0, // 0th register is hardwired zero
			_ => self.x[reg as usize]
		}
	}

	/// Reads Program counter content
	pub fn read_pc(&self) -> u64 {
		self.pc
	}

	// risc-box patch: true while the hart is parked in WFI with no enabled
	// interrupt pending (the same condition tick_operate uses to leave WFI).
	// Lets an embedder throttle ticking when the guest is idle.
	pub fn is_idle(&self) -> bool {
		self.wfi && (self.read_csr_raw(CSR_MIE_ADDRESS) & self.read_csr_raw(CSR_MIP_ADDRESS)) == 0
	}

	/// Runs program one cycle. Fetch, decode, and execution are completed in a cycle so far.
	// risc-box patch: tick() is now the single-instruction form of run() —
	// kept for the tests and for callers that need per-instruction stepping
	// (boot-bench's tracer).
	pub fn tick(&mut self) {
		self.run(1);
	}

	/// risc-box patch: runs `n` instructions with the loop bookkeeping hoisted
	/// out of the per-instruction path. Semantically this is n calls to the
	/// old tick():
	/// - devices and interrupt delivery used to run on every retired
	///   instruction — six device ticks plus two CSR reads, for an instruction
	///   whose own work is a tag compare and an indirect call. Both run every
	///   DEVICE_TICK_INTERVAL instructions, with the device clocks advanced by
	///   the whole interval so guest time passes at exactly the old rate, just
	///   in coarser steps.
	/// - interrupt delivery is not purely periodic: any CSR write that can
	///   change what is pending or enabled re-arms the check (see
	///   write_csr_raw), so enabling an already-pending interrupt still takes
	///   effect on the next instruction rather than waiting out the interval.
	/// - a hart parked in WFI consumes guest time without executing: the
	///   whole burst until the next device service is charged in one step, so
	///   an idle guest costs the host almost nothing while waking at exactly
	///   the same clint/plic boundaries as before.
	/// - CSR_CYCLE is materialized lazily in read_csr_raw() (same pattern as
	///   CSR_TIME) instead of being written every tick.
	pub fn run(&mut self, n: u64) {
		let mut remaining = n;
		// Superblocks retire several instructions per dispatch, so a call
		// stepping fewer instructions than a block might hold has to stay on
		// the single-instruction path — tick()/run(1) keeps exact stepping
		// for the tests and boot-bench's tracer.
		let allow_blocks = n >= BLOCK_MAX as u64;
		while remaining > 0 {
			// since_service < DEVICE_TICK_INTERVAL here (the service block
			// below resets it), so every burst makes progress.
			let until_service = DEVICE_TICK_INTERVAL - self.since_service;
			let burst = match remaining < until_service {
				true => remaining,
				false => until_service
			};
			let mut done: u64 = 0;
			if self.wfi {
				// Parked: leave WFI the moment an enabled interrupt is
				// pending (tick_operate's own wake condition); otherwise the
				// whole burst passes as guest time with no execution.
				match (self.read_csr_raw(CSR_MIE_ADDRESS)
					& self.read_csr_raw(CSR_MIP_ADDRESS)) != 0 {
					true => self.wfi = false,
					false => done = burst
				}
			}
			while done < burst {
				if allow_blocks {
					let slot = ((self.pc >> 1) as usize) & (BLOCK_SLOTS - 1);
					let h = self.block_heads[slot];
					let hit = h.tag == self.pc
						&& h.code_gen == self.mmu.code_gen()
						&& match self.mmu.translate_fetch_probe(self.pc) {
							Ok(p) => (p & !0xfff) == h.phys_page,
							Err(_) => false
						};
					if hit {
						#[cfg(feature = "tier2")]
						let compiled = {
							let g = self.mmu.code_gen();
							match self.tier2.as_mut() {
								Some(t) => t.t2.lookup(h.tag, g),
								None => None,
							}
						};
						#[cfg(feature = "aot")]
						if let Some((hh, idx)) = compiled {
							let g = self.mmu.code_gen();
							let fuel = burst - done;
							let ran = AOT_FNS[hh as usize](self, fuel, idx, g);
							if ran > 0 {
								if let Some(t) = self.tier2.as_mut() {
									t.total += ran;
									t.covered += ran;
									t.t2.note_break();
								}
								done += ran;
								if self.check_interrupt {
									self.check_interrupt = false;
									self.handle_interrupt(self.pc);
								}
								continue;
							}
						}
						let r = self.exec_block(slot);
						#[cfg(feature = "blockstats")]
						{
							self.stat_execs[slot] += 1;
							self.stat_retired[slot] += r;
							self.stat_note_block(h.tag, r);
						}
						#[cfg(feature = "tier2")]
						{
							if let Some(t) = self.tier2.as_mut() {
								t.total += r;
								if compiled.is_some() {
									t.covered += r;
								}
								let w = (t.total >> 22) & 7 == 0;
								if w && !t.window {
									// fresh window: never chain an edge
									// across the unrecorded gap
									t.t2.note_break();
								}
								t.window = w;
								match w {
									true => t.t2.note_block(h.tag, r),
									false => t.t2.note_retire(r),
								}
							}
						}
						done += r;
					} else if (self.pc & 0xfff) <= 0xff8 && self.build_block(slot) {
						let r = self.exec_block(slot);
						#[cfg(feature = "blockstats")]
						{
							self.stat_execs[slot] += 1;
							self.stat_retired[slot] += r;
							let tag = self.block_heads[slot].tag;
							self.stat_note_block(tag, r);
						}
						#[cfg(feature = "tier2")]
						{
							let g = self.mmu.code_gen();
							let tag = self.block_heads[slot].tag;
							if let Some(t) = self.tier2.as_mut() {
								t.total += r;
								if t.t2.lookup(tag, g).is_some() {
									t.covered += r;
								}
								let w = (t.total >> 22) & 7 == 0;
								if w && !t.window {
									t.t2.note_break();
								}
								t.window = w;
								match w {
									true => t.t2.note_block(tag, r),
									false => t.t2.note_retire(r),
								}
							}
						}
						done += r;
					} else {
						let instruction_address = self.pc;
						match self.tick_operate() {
							Ok(()) => {},
							Err(e) => self.handle_exception(e, instruction_address)
						}
						#[cfg(feature = "blockstats")]
						{
							self.stat_singlestep += 1;
							self.stat_prev = 0; // region chain broken
						}
						#[cfg(feature = "tier2")]
						if let Some(t) = self.tier2.as_mut() {
							t.total += 1;
							t.t2.note_break();
						}
						done += 1;
					}
				} else {
					let instruction_address = self.pc;
					match self.tick_operate() {
						Ok(()) => {},
						Err(e) => self.handle_exception(e, instruction_address)
					}
					done += 1;
				}
				// Delivery stays where the old tick() had it — after the
				// retired instruction that armed it. Nothing inside a block
				// can arm the check (CSR writes are terminal ops), so the
				// boundary a block ends on is the same one single-stepping
				// would deliver at.
				if self.check_interrupt {
					self.check_interrupt = false;
					self.handle_interrupt(self.pc);
				}
				if self.wfi {
					// the rest of the burst is idle time; charged as such by
					// the wfi branch of the next outer iteration
					break;
				}
			}
			self.since_service += done;
			self.clock = self.clock.wrapping_add(done);
			remaining = remaining.saturating_sub(done);
			// Device-service boundary, at the same stream position as the
			// old per-tick countdown: the end of the interval's last
			// instruction, delivery attempted in the same step. Blocks may
			// overshoot the boundary by up to BLOCK_MAX-1 instructions; the
			// device clocks advance by the true retired count either way,
			// so guest time stays tied to instructions retired.
			if self.since_service >= DEVICE_TICK_INTERVAL {
				let served = self.since_service;
				self.since_service = 0;
				self.mmu.tick(served, &mut self.csr[CSR_MIP_ADDRESS as usize]);
				self.check_interrupt = false;
				self.handle_interrupt(self.pc);
				#[cfg(feature = "tier2")]
				if self.tier2.as_ref().map_or(false, |t| t.t2.due()) {
					self.tier2_form_pass();
				}
			}
		}
	}

	/// risc-box patch: executes the block at `slot` (probe already matched).
	/// Returns instructions retired (>= 1). Exits early — with pc exact —
	/// on a trap, a taken branch/jump, or a hot store that invalidated the
	/// block's own meta (self-modifying code).
	/// risc-box patch (jit feature tests): install a block directly so the
	/// translator's equivalence tests can drive exec_block on hand-built op
	/// sequences without going through fetch/decode.
	#[cfg(feature = "jit")]
	pub(crate) fn install_block_for_test(&mut self, slot: usize, tag: u64, phys_page: u64, ops: &[BlockOp]) {
		let base = slot * BLOCK_MAX;
		for (i, op) in ops.iter().enumerate() {
			self.block_ops[base + i] = *op;
		}
		self.block_heads[slot] = BlockHead {
			tag: tag,
			phys_page: phys_page,
			count: ops.len() as u32,
			code_gen: self.mmu.code_gen()
		};
	}

	/// risc-box patch (tier2): switch the dispatcher on, dumping every
	/// formed region to `dump` when given (the AOT bake pipeline's input).
	#[cfg(feature = "tier2")]
	pub fn tier2_enable(&mut self, dump: Option<&std::path::Path>) {
		let mut t2 = ::jit::Tier2::new(Box::new(::jit::RecordBackend::new(dump)));
		Self::tier2_tune(&mut t2);
		self.tier2 = Some(Box::new(Tier2State {
			t2,
			lay: Self::tier2_layout(),
			covered: 0,
			total: 0,
			window: false,
			passes: 0,
		}));
	}

	/// One tuning for the profiling run AND the shipped dispatcher — the
	/// baked-region hash match depends on both forming the same regions
	/// from the same knobs and the same sampling.
	#[cfg(feature = "tier2")]
	fn tier2_tune(t2: &mut ::jit::Tier2) {
		t2.greedy = true;
		t2.max_blocks = 96;
		// heat is sampled 1-in-8 service intervals, so thresholds are an
		// eighth of their full-rate meaning: 20k sampled ~ 160k true
		t2.min_heat = 20_000;
	}

	/// risc-box patch (aot): dispatcher over the BAKED region tables. A
	/// formation that hashes to something unbaked just stays interpreted —
	/// and is not blacklisted, so a later, differently-shaped formation of
	/// the same code still gets its chance to match.
	#[cfg(feature = "aot")]
	pub fn aot_enable(&mut self) {
		let mut t2 = ::jit::Tier2::new(Box::new(AotBackend));
		Self::tier2_tune(&mut t2);
		t2.blacklist_on_fail = false;
		self.tier2 = Some(Box::new(Tier2State {
			t2,
			lay: Self::tier2_layout(),
			covered: 0,
			total: 0,
			window: false,
			passes: 0,
		}));
	}

	#[cfg(feature = "aot")]
	pub fn aot_baked(&self) -> usize {
		AOT_FNS.len()
	}

	/// The production Layout the runtime dispatcher hands emit_region. For
	/// coverage/bake runs only the DETERMINISM matters (the module hash is
	/// the match key), so the values just have to be fixed and plausible.
	#[cfg(feature = "tier2")]
	fn tier2_layout() -> ::jit::Layout {
		::jit::Layout {
			x_base: 0,
			f_base: 512,
			tlb: None,
			pc_addr: 256,
			gen_addr: 264,
			baked_gen: 0,
			dram_base: 4096,
			guest_dram_base: 0x8000_0000,
			dram_len: 1 << 31,
		}
	}

	/// (covered, total, compiled entry pcs, blacklisted pcs)
	#[cfg(feature = "tier2")]
	pub fn tier2_stats(&self) -> (u64, u64, usize, usize) {
		match self.tier2.as_ref() {
			Some(t) => {
				let (c, b) = t.t2.sizes();
				(t.covered, t.total, c, b)
			}
			None => (0, 0, 0, 0),
		}
	}

	#[cfg(feature = "tier2")]
	fn tier2_form_pass(&mut self) {
		let Some(mut t) = self.tier2.take() else { return };
		let gen = self.mmu.code_gen();
		{
			let Tier2State { ref mut t2, ref lay, .. } = *t;
			t2.maybe_form(lay, gen, |pc| self.tier2_ops_of(pc));
		}
		self.tier2 = Some(t);
	}

	/// A block's cached ops, exactly as the interpreter runs them — the
	/// region emitter's source of truth. None when the cache has moved on.
	#[cfg(feature = "tier2")]
	fn tier2_ops_of(&self, pc: u64) -> Option<(u64, Vec<BlockOp>)> {
		let slot = ((pc >> 1) as usize) & (BLOCK_SLOTS - 1);
		let h = self.block_heads[slot];
		if h.tag != pc || h.count == 0 {
			return None;
		}
		let base = slot * BLOCK_MAX;
		Some((pc, self.block_ops[base..base + h.count as usize].to_vec()))
	}

	pub(crate) fn exec_block(&mut self, slot: usize) -> u64 {
		let head = self.block_heads[slot];
		let base = slot * BLOCK_MAX;
		let count = head.count as usize;
		let mut retired: u64 = 0;
		for i in 0..count {
			let op = self.block_ops[base + i];
			let address = self.pc;
			let next = address.wrapping_add(op.len as u64);
			self.pc = next;
			let result = self.exec_op(&op, address);
			self.x[0] = 0; // hardwired zero
			retired += 1;
			match result {
				Ok(()) => {},
				Err(e) => {
					self.handle_exception(e, address);
					return retired;
				}
			}
			if self.pc != next {
				return retired; // taken branch/jump left the block
			}
			// hot stores (kind 1..=4) can overwrite this very block; the
			// write snoop bumps the code generation, which this meta embeds
			if op.kind <= HOT_STORE_MAX && self.mmu.code_gen() != head.code_gen {
				return retired;
			}
		}
		retired
	}

	#[cfg(feature = "blockstats")]
	fn stat_flush_slot(&mut self, slot: usize) {
		let e = self.stat_execs[slot];
		let r = self.stat_retired[slot];
		if e > 0 {
			let b = match e {
				1..=3 => 0,
				4..=15 => 1,
				16..=63 => 2,
				64..=255 => 3,
				256..=4095 => 4,
				_ => 5
			};
			self.stat_hist[b] += r;
			self.stat_execs[slot] = 0;
			self.stat_retired[slot] = 0;
		}
	}

	#[cfg(feature = "blockstats")]
	fn stat_note_block(&mut self, tag: u64, retired: u64) {
		let e = self.stat_nodes.entry(tag).or_insert((0, 0));
		e.0 += 1;
		e.1 += retired;
		if self.stat_prev != 0 && self.stat_edges.len() < 4_000_000 {
			*self.stat_edges.entry((self.stat_prev, tag)).or_insert(0) += 1;
		}
		self.stat_prev = tag;
	}

	/// risc-box patch (blockstats): iterative Tarjan SCC over the block
	/// graph; returns for each node index its SCC id, plus SCC sizes.
	#[cfg(feature = "blockstats")]
	fn stat_regions(&self) -> (Vec<u64>, Vec<(usize, u64, u64, Vec<u64>)>) {
		// index nodes
		let mut ids: Vec<u64> = self.stat_nodes.keys().cloned().collect();
		ids.sort();
		let index_of = |pc: u64| ids.binary_search(&pc).ok();
		let n = ids.len();
		let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
		let mut self_loop = vec![false; n];
		for (&(a, b), _) in self.stat_edges.iter() {
			// only LOCAL edges: intra-function branches. Calls and returns
			// jump far and would collapse the whole program into one SCC;
			// a region compiler wouldn't cross them either.
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
		// per-SCC: node count, execs, retired, member pcs (capped)
		let mut sccs: Vec<(usize, u64, u64, Vec<u64>)> = vec![(0, 0, 0, Vec::new()); scc_count as usize];
		let mut node_scc = vec![0u64; n];
		for i in 0..n {
			let sid = scc_of[i] as usize;
			let (ex, rt) = self.stat_nodes[&ids[i]];
			sccs[sid].0 += 1;
			sccs[sid].1 += ex;
			sccs[sid].2 += rt;
			if sccs[sid].3.len() < 8 {
				sccs[sid].3.push(ids[i]);
			}
			// cyclic if SCC has >1 node or the node self-loops
			node_scc[i] = match sccs[sid].0 > 1 || self_loop[i] {
				true => 1,
				false => 0
			};
		}
		// second pass: a node joined before its SCC grew past 1 needs the flag
		for i in 0..n {
			let sid = scc_of[i] as usize;
			if sccs[sid].0 > 1 || self_loop[i] {
				node_scc[i] = 1;
			}
		}
		// retired mass by cyclicity
		let mut cyc = 0u64;
		let mut lin = 0u64;
		for i in 0..n {
			let (_, rt) = self.stat_nodes[&ids[i]];
			match node_scc[i] {
				1 => cyc += rt,
				_ => lin += rt
			}
		}
		(vec![cyc, lin], sccs)
	}

	/// risc-box patch (blockstats): flush live slots and print the coverage
	/// histogram: retired instructions bucketed by the block's execution
	/// count, plus the single-step share.
	#[cfg(feature = "blockstats")]
	pub fn dump_block_stats(&mut self) {
		for slot in 0..BLOCK_SLOTS {
			self.stat_flush_slot(slot);
		}
		let total: u64 = self.stat_hist.iter().sum::<u64>() + self.stat_singlestep;
		let names = ["execs 1-3", "execs 4-15", "execs 16-63", "execs 64-255", "execs 256-4095", "execs 4096+"];
		eprintln!("block coverage (retired instructions by block hotness):");
		for i in 0..6 {
			eprintln!("  {:>15}: {:>12}  {:>5.1}%", names[i], self.stat_hist[i],
				self.stat_hist[i] as f64 * 100.0 / total as f64);
		}
		eprintln!("  {:>15}: {:>12}  {:>5.1}%", "single-step", self.stat_singlestep,
			self.stat_singlestep as f64 * 100.0 / total as f64);
		// region/loop discovery over the recorded block graph
		let (mass, mut sccs) = self.stat_regions();
		let node_total: u64 = mass[0] + mass[1];
		eprintln!("block graph: {} nodes, {} edges", self.stat_nodes.len(), self.stat_edges.len());
		eprintln!("  in-cycle retired mass: {:>12}  {:>5.1}%", mass[0],
			mass[0] as f64 * 100.0 / node_total as f64);
		eprintln!("  straight-line mass:    {:>12}  {:>5.1}%", mass[1],
			mass[1] as f64 * 100.0 / node_total as f64);
		sccs.sort_by(|a, b| b.2.cmp(&a.2));
		eprintln!("  top cyclic regions (blocks, execs, retired):");
		let mut shown = 0;
		for (nn, ex, rt, pcs) in sccs.iter() {
			if *nn > 1 && shown < 8 {
				let hex: Vec<String> = pcs.iter().map(|p| format!("{:#x}", p)).collect();
				eprintln!("    {:>4} blocks  {:>12} execs  {:>12} retired ({:.1}%)  pcs: {}",
					nn, ex, rt, *rt as f64 * 100.0 / node_total as f64, hex.join(" "));
				shown += 1;
			}
		}
	}

	/// risc-box patch: builds a block starting at the current pc into
	/// `slot`. Returns false when no block can be built here (page-tail
	/// start, fetch fault, executing outside DRAM, or an undecodable first
	/// word) — the caller falls back to single-stepping.
	fn build_block(&mut self, slot: usize) -> bool {
		let start = self.pc;
		let p_start = match self.mmu.translate_fetch(start) {
			Ok(p) => p,
			Err(_) => return false
		};
		if !self.mmu.mark_exec_page(p_start) {
			return false;
		}
		let base = slot * BLOCK_MAX;
		let mut pc = start;
		let mut count = 0usize;
		while count < BLOCK_MAX && (pc & 0xfff) <= 0xff8 {
			// same page as start, so the frame is the translation we already
			// have — no per-op walk
			let p = (p_start & !0xfff) | (pc & 0xfff);
			let original_word = self.mmu.load_word_raw(p);
			let (word, len) = match (original_word & 0x3) == 0x3 {
				true => (original_word, 4u8),
				false => (self.uncompress(original_word & 0xffff), 2u8)
			};
			let index = match self.decode_cache.get(word) {
				Some(index) => index,
				None => match self.decode_and_get_instruction_index(word) {
					Ok(index) => {
						self.decode_cache.insert(word, index);
						index
					},
					// Undecodable: stop the block before it so the illegal
					// instruction raises through the ordinary path with its
					// pc exact.
					Err(()) => break
				}
			};
			let (kind, rd, rs1, rs2, imm) = classify_hot(INSTRUCTIONS[index].name, word);
			self.block_ops[base + count] = BlockOp {
				imm: imm,
				word: word,
				data: index as u16
					| (match len { 4 => ICACHE_LEN4, _ => 0 }),
				kind: kind,
				rd: rd,
				rs1: rs1,
				rs2: rs2,
				len: len,
				_pad: 0
			};
			count += 1;
			pc = pc.wrapping_add(len as u64);
			// Terminal ops: a non-hot instruction may change interrupt or
			// translation state (it must stay the block's last op), and
			// JAL/JALR always leave, so anything after them is unreachable.
			if kind == 0 || kind == HOT_JAL || kind == HOT_JALR {
				break;
			}
		}
		if count == 0 {
			return false;
		}
		#[cfg(feature = "blockstats")]
		self.stat_flush_slot(slot);
		self.block_heads[slot] = BlockHead {
			tag: start,
			phys_page: p_start & !0xfff,
			count: count as u32,
			code_gen: self.mmu.code_gen()
		};
		true
	}

	// @TODO: Rename?
	fn tick_operate(&mut self) -> Result<(), Trap> {
		if self.wfi {
			if (self.read_csr_raw(CSR_MIE_ADDRESS) &
				self.read_csr_raw(CSR_MIP_ADDRESS)) != 0{
				self.wfi = false;
			}
			return Ok(());
		}

		// risc-box patch: the predecoded fast path lives in run()'s block
		// cache now; this is the exact single-step used by tick()/run(1),
		// page-tail pcs and block-build failures.
		let original_word = match self.fetch() {
			Ok(word) => word,
			Err(e) => return Err(e)
		};
		let instruction_address = self.pc;
		let word = match (original_word & 0x3) == 0x3 {
			true => {
				self.pc = self.pc.wrapping_add(4); // 32-bit length non-compressed instruction
				original_word
			},
			false => {
				self.pc = self.pc.wrapping_add(2); // 16-bit length compressed instruction
				self.uncompress(original_word & 0xffff)
			}
		};

		// risc-box patch: decode to an INSTRUCTIONS index (decode() would only
		// give the reference; the fill below needs the index).
		let index = match self.decode_cache.get(word) {
			Some(index) => index,
			None => match self.decode_and_get_instruction_index(word) {
				Ok(index) => {
					self.decode_cache.insert(word, index);
					index
				},
				Err(()) => {
					// risc-box patch: an undecodable word must not abort the
					// embedding host app. Raise illegal-instruction like real
					// silicon: the guest kernel SIGILLs the process (or
					// emulates) and the machine lives on. tval carries the
					// faulting word, epc the address (set by the caller).
					log_illegal(self.pc, original_word);
					return Err(Trap {
						trap_type: TrapType::IllegalInstruction,
						value: original_word as u64
					});
				}
			}
		};

		let result = (INSTRUCTIONS[index].operation)(self, word, instruction_address);
		self.x[0] = 0; // hardwired zero
		result
	}

	// risc-box patch: inline execution of the predecoded hot set. Every arm
	// is the corresponding INSTRUCTIONS closure body verbatim, with the
	// parse_format_* call replaced by the op's build-time fields (shift
	// amounts still come from the word so the xlen-dependent masks run
	// exactly as upstream wrote them). Keeping the bodies identical is the
	// correctness argument: this is the same code, minus re-parsing and an
	// indirect call. kind 0 (the block's terminal non-hot op) dispatches
	// through the table.
	#[inline(always)]
	pub(crate) fn exec_op(&mut self, e: &BlockOp, address: u64) -> Result<(), Trap> {
		let rd = e.rd as usize;
		let rs1 = e.rs1 as usize;
		let rs2 = e.rs2 as usize;
		let imm = e.imm as i64;
		match e.kind {
			HOT_ADDI => {
				self.x[rd] = self.sign_extend(self.x[rs1].wrapping_add(imm));
			},
			HOT_ADD => {
				self.x[rd] = self.sign_extend(self.x[rs1].wrapping_add(self.x[rs2]));
			},
			HOT_LD => {
				self.x[rd] = match self.mmu.load_doubleword(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_SD => {
				return self.mmu.store_doubleword(self.x[rs1].wrapping_add(imm) as u64, self.x[rs2] as u64);
			},
			HOT_LW => {
				self.x[rd] = match self.mmu.load_word(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i32 as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_SW => {
				return self.mmu.store_word(self.x[rs1].wrapping_add(imm) as u64, self.x[rs2] as u32);
			},
			HOT_BEQ => {
				if self.sign_extend(self.x[rs1]) == self.sign_extend(self.x[rs2]) {
					self.pc = address.wrapping_add(imm as u64);
				}
			},
			HOT_BNE => {
				if self.sign_extend(self.x[rs1]) != self.sign_extend(self.x[rs2]) {
					self.pc = address.wrapping_add(imm as u64);
				}
			},
			HOT_BLT => {
				if self.sign_extend(self.x[rs1]) < self.sign_extend(self.x[rs2]) {
					self.pc = address.wrapping_add(imm as u64);
				}
			},
			HOT_BGE => {
				if self.sign_extend(self.x[rs1]) >= self.sign_extend(self.x[rs2]) {
					self.pc = address.wrapping_add(imm as u64);
				}
			},
			HOT_BLTU => {
				if self.unsigned_data(self.x[rs1]) < self.unsigned_data(self.x[rs2]) {
					self.pc = address.wrapping_add(imm as u64);
				}
			},
			HOT_BGEU => {
				if self.unsigned_data(self.x[rs1]) >= self.unsigned_data(self.x[rs2]) {
					self.pc = address.wrapping_add(imm as u64);
				}
			},
			HOT_LUI => {
				self.x[rd] = imm;
			},
			HOT_AUIPC => {
				self.x[rd] = self.sign_extend(address.wrapping_add(imm as u64) as i64);
			},
			HOT_JAL => {
				self.x[rd] = self.sign_extend(self.pc as i64);
				self.pc = address.wrapping_add(imm as u64);
			},
			HOT_JALR => {
				let tmp = self.sign_extend(self.pc as i64);
				self.pc = (self.x[rs1] as u64).wrapping_add(imm as u64);
				self.x[rd] = tmp;
			},
			HOT_ANDI => {
				self.x[rd] = self.sign_extend(self.x[rs1] & imm);
			},
			HOT_ORI => {
				self.x[rd] = self.sign_extend(self.x[rs1] | imm);
			},
			HOT_XORI => {
				self.x[rd] = self.sign_extend(self.x[rs1] ^ imm);
			},
			HOT_AND => {
				self.x[rd] = self.sign_extend(self.x[rs1] & self.x[rs2]);
			},
			HOT_OR => {
				self.x[rd] = self.sign_extend(self.x[rs1] | self.x[rs2]);
			},
			HOT_XOR => {
				self.x[rd] = self.sign_extend(self.x[rs1] ^ self.x[rs2]);
			},
			HOT_SUB => {
				self.x[rd] = self.sign_extend(self.x[rs1].wrapping_sub(self.x[rs2]));
			},
			HOT_SLLI => {
				let mask = match self.xlen {
					Xlen::Bit32 => 0x1f,
					Xlen::Bit64 => 0x3f
				};
				let shamt = (e.word >> 20) & mask;
				self.x[rd] = self.sign_extend(self.x[rs1] << shamt);
			},
			HOT_SRLI => {
				let mask = match self.xlen {
					Xlen::Bit32 => 0x1f,
					Xlen::Bit64 => 0x3f
				};
				let shamt = (e.word >> 20) & mask;
				self.x[rd] = self.sign_extend((self.unsigned_data(self.x[rs1]) >> shamt) as i64);
			},
			HOT_SRAI => {
				let mask = match self.xlen {
					Xlen::Bit32 => 0x1f,
					Xlen::Bit64 => 0x3f
				};
				let shamt = (e.word >> 20) & mask;
				self.x[rd] = self.sign_extend(self.x[rs1] >> shamt);
			},
			HOT_ADDIW => {
				self.x[rd] = self.x[rs1].wrapping_add(imm) as i32 as i64;
			},
			HOT_ADDW => {
				self.x[rd] = self.x[rs1].wrapping_add(self.x[rs2]) as i32 as i64;
			},
			HOT_SUBW => {
				self.x[rd] = self.x[rs1].wrapping_sub(self.x[rs2]) as i32 as i64;
			},
			HOT_SLLIW => {
				let shamt = e.rs2 as u32;
				self.x[rd] = (self.x[rs1] << shamt) as i32 as i64;
			},
			HOT_SRLIW => {
				let mask = match self.xlen {
					Xlen::Bit32 => 0x1f,
					Xlen::Bit64 => 0x3f
				};
				let shamt = (e.word >> 20) & mask;
				self.x[rd] = ((self.x[rs1] as u32) >> shamt) as i32 as i64;
			},
			HOT_SRAIW => {
				let shamt = ((e.word >> 20) & 0x1f) as u32;
				self.x[rd] = ((self.x[rs1] as i32) >> shamt) as i64;
			},
			HOT_SLLW => {
				self.x[rd] = (self.x[rs1] as u32).wrapping_shl(self.x[rs2] as u32) as i32 as i64;
			},
			HOT_SRLW => {
				self.x[rd] = (self.x[rs1] as u32).wrapping_shr(self.x[rs2] as u32) as i32 as i64;
			},
			HOT_SRAW => {
				self.x[rd] = (self.x[rs1] as i32).wrapping_shr(self.x[rs2] as u32) as i64;
			},
			HOT_SLL => {
				self.x[rd] = self.sign_extend(self.x[rs1].wrapping_shl(self.x[rs2] as u32));
			},
			HOT_SRL => {
				self.x[rd] = self.sign_extend(self.unsigned_data(self.x[rs1]).wrapping_shr(self.x[rs2] as u32) as i64);
			},
			HOT_SRA => {
				self.x[rd] = self.sign_extend(self.x[rs1].wrapping_shr(self.x[rs2] as u32));
			},
			HOT_SLT => {
				self.x[rd] = match self.x[rs1] < self.x[rs2] {
					true => 1,
					false => 0
				};
			},
			HOT_SLTI => {
				self.x[rd] = match self.x[rs1] < imm {
					true => 1,
					false => 0
				};
			},
			HOT_SLTU => {
				self.x[rd] = match self.unsigned_data(self.x[rs1]) < self.unsigned_data(self.x[rs2]) {
					true => 1,
					false => 0
				};
			},
			HOT_SLTIU => {
				self.x[rd] = match self.unsigned_data(self.x[rs1]) < self.unsigned_data(imm) {
					true => 1,
					false => 0
				};
			},
			HOT_MUL => {
				self.x[rd] = self.sign_extend(self.x[rs1].wrapping_mul(self.x[rs2]));
			},
			HOT_LB => {
				self.x[rd] = match self.mmu.load(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i8 as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_LBU => {
				self.x[rd] = match self.mmu.load(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_LH => {
				self.x[rd] = match self.mmu.load_halfword(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i16 as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_LHU => {
				self.x[rd] = match self.mmu.load_halfword(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_LWU => {
				self.x[rd] = match self.mmu.load_word(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => data as i64,
					Err(e) => return Err(e)
				};
			},
			HOT_SB => {
				return self.mmu.store(self.x[rs1].wrapping_add(imm) as u64, self.x[rs2] as u8);
			},
			HOT_SH => {
				return self.mmu.store_halfword(self.x[rs1].wrapping_add(imm) as u64, self.x[rs2] as u16);
			},
			HOT_FSW => {
				return self.mmu.store_word(self.x[rs1].wrapping_add(imm) as u64, self.f[rs2].to_bits() as u32);
			},
			HOT_FSD => {
				return self.mmu.store_doubleword(self.x[rs1].wrapping_add(imm) as u64, self.f[rs2].to_bits());
			},
			HOT_FLD => {
				self.f[rd] = match self.mmu.load_doubleword(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => f64::from_bits(data),
					Err(e) => return Err(e)
				};
			},
			HOT_FLW => {
				self.f[rd] = match self.mmu.load_word(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => f64::from_bits(data as i32 as i64 as u64),
					Err(e) => return Err(e)
				};
			},
			HOT_FADD_D => {
				self.f[rd] = self.f[rs1] + self.f[rs2];
			},
			HOT_FSUB_D => {
				self.f[rd] = self.f[rs1] - self.f[rs2];
			},
			HOT_FMUL_D => {
				self.f[rd] = self.f[rs1] * self.f[rs2];
			},
			HOT_FDIV_D => {
				let dividend = self.f[rs1];
				let divisor = self.f[rs2];
				// Is this implementation correct? (verbatim from the table)
				if divisor == 0.0 {
					self.f[rd] = std::f64::INFINITY;
					self.set_fcsr_dz();
				} else if divisor == -0.0 {
					self.f[rd] = std::f64::NEG_INFINITY;
					self.set_fcsr_dz();
				} else {
					self.f[rd] = dividend / divisor;
				}
			},
			HOT_FSGNJ_D => {
				let rs1_bits = self.f[rs1].to_bits();
				let rs2_bits = self.f[rs2].to_bits();
				let sign_bit = rs2_bits & 0x8000000000000000;
				self.f[rd] = f64::from_bits(sign_bit | (rs1_bits & 0x7fffffffffffffff));
			},
			HOT_FMV_X_D => {
				self.x[rd] = self.f[rs1].to_bits() as i64;
			},
			HOT_FMV_D_X => {
				self.f[rd] = f64::from_bits(self.x[rs1] as u64);
			},
			HOT_FCVT_D_W => {
				self.f[rd] = self.x[rs1] as i32 as f64;
			},
			// kind is only ever written by classify_hot, so this arm is dead;
			// the table dispatch (not a panic — a guest must never crash the
			// host) keeps it safe anyway.
			_ => {
				return (INSTRUCTIONS[(e.data & !ICACHE_LEN4) as usize].operation)(
					self, e.word, address);
			}
		}
		Ok(())
	}

	/// Decodes a word instruction data and returns a reference to
	/// [`Instruction`](struct.Instruction.html). Using [`DecodeCache`](struct.DecodeCache.html)
	/// so if cache hits this method returns the result very quickly.
	/// The result will be stored to cache.
	// risc-box patch: tick_operate now decodes inline (it needs the index for
	// the predecode cache); this remains for the unit tests.
	#[allow(dead_code)]
	fn decode(&mut self, word: u32) -> Result<&Instruction, ()> {
		match self.decode_cache.get(word) {
			Some(index) => return Ok(&INSTRUCTIONS[index]),
			None => match self.decode_and_get_instruction_index(word) {
				Ok(index) => {
					self.decode_cache.insert(word, index);
					Ok(&INSTRUCTIONS[index])
				},
				Err(()) => Err(())
			}
		}
	}

	/// Decodes a word instruction data and returns a reference to
	/// [`Instruction`](struct.Instruction.html). Not Using [`DecodeCache`](struct.DecodeCache.html)
	/// so if you don't want to pollute the cache you should use this method
	/// instead of `decode`.
	fn decode_raw(&self, word: u32) -> Result<&Instruction, ()> {
		match self.decode_and_get_instruction_index(word) {
			Ok(index) => Ok(&INSTRUCTIONS[index]),
			Err(()) => Err(())
		}
	}

	/// Decodes a word instruction data and returns an index of
	/// [`INSTRUCTIONS`](constant.INSTRUCTIONS.html)
	///
	/// # Arguments
	/// * `word` word instruction data decoded
	fn decode_and_get_instruction_index(&self, word: u32) -> Result<usize, ()> {
		for i in 0..INSTRUCTION_NUM {
			let inst = &INSTRUCTIONS[i];
			if (word & inst.mask) == inst.data {
				return Ok(i);
			}
		}
		return Err(())
	}

	fn handle_interrupt(&mut self, instruction_address: u64) {
		// @TODO: Optimize
		let minterrupt = self.read_csr_raw(CSR_MIP_ADDRESS) & self.read_csr_raw(CSR_MIE_ADDRESS);

		if (minterrupt & MIP_MEIP) != 0 {
			if self.handle_trap(Trap {
				trap_type: TrapType::MachineExternalInterrupt,
				value: self.pc // dummy
			}, instruction_address, true) {
				// Who should clear mip bit?
				self.write_csr_raw(CSR_MIP_ADDRESS, self.read_csr_raw(CSR_MIP_ADDRESS) & !MIP_MEIP);
				self.wfi = false;
				return;
			}
		}
		if (minterrupt & MIP_MSIP) != 0 {
			if self.handle_trap(Trap {
				trap_type: TrapType::MachineSoftwareInterrupt,
				value: self.pc // dummy
			}, instruction_address, true) {
				self.write_csr_raw(CSR_MIP_ADDRESS, self.read_csr_raw(CSR_MIP_ADDRESS) & !MIP_MSIP);
				self.wfi = false;
				return;
			}
		}
		if (minterrupt & MIP_MTIP) != 0 {
			if self.handle_trap(Trap {
				trap_type: TrapType::MachineTimerInterrupt,
				value: self.pc // dummy
			}, instruction_address, true) {
				self.write_csr_raw(CSR_MIP_ADDRESS, self.read_csr_raw(CSR_MIP_ADDRESS) & !MIP_MTIP);
				self.wfi = false;
				return;
			}
		}
		if (minterrupt & MIP_SEIP) != 0 {
			if self.handle_trap(Trap {
				trap_type: TrapType::SupervisorExternalInterrupt,
				value: self.pc // dummy
			}, instruction_address, true) {
				self.write_csr_raw(CSR_MIP_ADDRESS, self.read_csr_raw(CSR_MIP_ADDRESS) & !MIP_SEIP);
				self.wfi = false;
				return;
			}
		}
		if (minterrupt & MIP_SSIP) != 0 {
			if self.handle_trap(Trap {
				trap_type: TrapType::SupervisorSoftwareInterrupt,
				value: self.pc // dummy
			}, instruction_address, true) {
				self.write_csr_raw(CSR_MIP_ADDRESS, self.read_csr_raw(CSR_MIP_ADDRESS) & !MIP_SSIP);
				self.wfi = false;
				return;
			}
		}
		if (minterrupt & MIP_STIP) != 0 {
			if self.handle_trap(Trap {
				trap_type: TrapType::SupervisorTimerInterrupt,
				value: self.pc // dummy
			}, instruction_address, true) {
				self.write_csr_raw(CSR_MIP_ADDRESS, self.read_csr_raw(CSR_MIP_ADDRESS) & !MIP_STIP);
				self.wfi = false;
				return;
			}
		}
	}

	fn handle_exception(&mut self, exception: Trap, instruction_address: u64) {
		self.handle_trap(exception, instruction_address, false);
	}

	fn handle_trap(&mut self, trap: Trap, instruction_address: u64, is_interrupt: bool) -> bool{
		let current_privilege_encoding = get_privilege_encoding(&self.privilege_mode) as u64;
		let cause = get_trap_cause(&trap, &self.xlen);

		// First, determine which privilege mode should handle the trap.
		// @TODO: Check if this logic is correct
		let mdeleg = match is_interrupt {
			true => self.read_csr_raw(CSR_MIDELEG_ADDRESS),
			false => self.read_csr_raw(CSR_MEDELEG_ADDRESS)
		};
		let sdeleg = match is_interrupt {
			true => self.read_csr_raw(CSR_SIDELEG_ADDRESS),
			false => self.read_csr_raw(CSR_SEDELEG_ADDRESS)
		};
		let pos = cause & 0xffff;

		let new_privilege_mode = match ((mdeleg >> pos) & 1) == 0 {
			true => PrivilegeMode::Machine,
			false => match ((sdeleg >> pos) & 1) == 0 {
				true => PrivilegeMode::Supervisor,
				false => PrivilegeMode::User
			}
		};
		let new_privilege_encoding = get_privilege_encoding(&new_privilege_mode) as u64;

		let current_status = match self.privilege_mode {
			PrivilegeMode::Machine => self.read_csr_raw(CSR_MSTATUS_ADDRESS),
			PrivilegeMode::Supervisor => self.read_csr_raw(CSR_SSTATUS_ADDRESS),
			PrivilegeMode::User => self.read_csr_raw(CSR_USTATUS_ADDRESS),
			PrivilegeMode::Reserved => panic!(),
		};

		// Second, ignore the interrupt if it's disabled by some conditions

		if is_interrupt {
			let ie = match new_privilege_mode {
				PrivilegeMode::Machine => self.read_csr_raw(CSR_MIE_ADDRESS),
				PrivilegeMode::Supervisor => self.read_csr_raw(CSR_SIE_ADDRESS),
				PrivilegeMode::User => self.read_csr_raw(CSR_UIE_ADDRESS),
				PrivilegeMode::Reserved => panic!(),
			};

			let current_mie = (current_status >> 3) & 1;
			let current_sie = (current_status >> 1) & 1;
			let current_uie = current_status & 1;

			let msie = (ie >> 3) & 1;
			let ssie = (ie >> 1) & 1;
			let usie = ie & 1;

			let mtie = (ie >> 7) & 1;
			let stie = (ie >> 5) & 1;
			let utie = (ie >> 4) & 1;

			let meie = (ie >> 11) & 1;
			let seie = (ie >> 9) & 1;
			let ueie = (ie >> 8) & 1;

			// 1. Interrupt is always enabled if new privilege level is higher
			// than current privilege level
			// 2. Interrupt is always disabled if new privilege level is lower
			// than current privilege level
			// 3. Interrupt is enabled if xIE in xstatus is 1 where x is privilege level
			// and new privilege level equals to current privilege level

			if new_privilege_encoding < current_privilege_encoding {
				return false;
			} else if current_privilege_encoding == new_privilege_encoding {
				match self.privilege_mode {
					PrivilegeMode::Machine => {
						if current_mie == 0 {
							return false;
						}
					},
					PrivilegeMode::Supervisor => {
						if current_sie == 0 {
							return false;
						}
					},
					PrivilegeMode::User => {
						if current_uie == 0 {
							return false;
						}
					},
					PrivilegeMode::Reserved => panic!()
				};
			}

			// Interrupt can be maskable by xie csr register
			// where x is a new privilege mode.

			match trap.trap_type {
				TrapType::UserSoftwareInterrupt => {
					if usie == 0 {
						return false;
					}
				},
				TrapType::SupervisorSoftwareInterrupt => {
					if ssie == 0 {
						return false;
					}
				},
				TrapType::MachineSoftwareInterrupt => {
					if msie == 0 {
						return false;
					}
				},
				TrapType::UserTimerInterrupt => {
					if utie == 0 {
						return false;
					}
				},
				TrapType::SupervisorTimerInterrupt => {
					if stie == 0 {
						return false;
					}
				},
				TrapType::MachineTimerInterrupt => {
					if mtie == 0 {
						return false;
					}
				},
				TrapType::UserExternalInterrupt => {
					if ueie == 0 {
						return false;
					}
				},
				TrapType::SupervisorExternalInterrupt => {
					if seie == 0 {
						return false;
					}
				},
				TrapType::MachineExternalInterrupt => {
					if meie == 0 {
						return false;
					}
				},
				_ => {}
			};
		}

		// So, this trap should be taken

		// risc-box patch: an LR/SC reservation must not survive a trap. On a
		// single emulated hart, every context switch passes through here; a
		// reservation that lives across the switch lets thread B's plain
		// stores go unnoticed and thread A's SC then succeeds against a stale
		// read -- a lost update. That silently corrupted CAS loops under
		// contention (V8's concurrent TurboFan thread vs its main thread:
		// the compiler read poisoned feedback and emitted wrong code).
		self.is_reservation_set = false;

		self.privilege_mode = new_privilege_mode;
		self.mmu.update_privilege_mode(self.privilege_mode.clone());
		let csr_epc_address = match self.privilege_mode {
			PrivilegeMode::Machine => CSR_MEPC_ADDRESS,
			PrivilegeMode::Supervisor => CSR_SEPC_ADDRESS,
			PrivilegeMode::User => CSR_UEPC_ADDRESS,
			PrivilegeMode::Reserved => panic!()
		};
		let csr_cause_address = match self.privilege_mode {
			PrivilegeMode::Machine => CSR_MCAUSE_ADDRESS,
			PrivilegeMode::Supervisor => CSR_SCAUSE_ADDRESS,
			PrivilegeMode::User => CSR_UCAUSE_ADDRESS,
			PrivilegeMode::Reserved => panic!()
		};
		let csr_tval_address = match self.privilege_mode {
			PrivilegeMode::Machine => CSR_MTVAL_ADDRESS,
			PrivilegeMode::Supervisor => CSR_STVAL_ADDRESS,
			PrivilegeMode::User => CSR_UTVAL_ADDRESS,
			PrivilegeMode::Reserved => panic!()
		};
		let csr_tvec_address = match self.privilege_mode {
			PrivilegeMode::Machine => CSR_MTVEC_ADDRESS,
			PrivilegeMode::Supervisor => CSR_STVEC_ADDRESS,
			PrivilegeMode::User => CSR_UTVEC_ADDRESS,
			PrivilegeMode::Reserved => panic!()
		};

		self.write_csr_raw(csr_epc_address, instruction_address);
		self.write_csr_raw(csr_cause_address, cause);
		self.write_csr_raw(csr_tval_address, trap.value);
		self.pc = self.read_csr_raw(csr_tvec_address);

		// Add 4 * cause if tvec has vector type address
		if (self.pc & 0x3) != 0 {
			self.pc = (self.pc & !0x3) + 4 * (cause & 0xffff);
		}

		match self.privilege_mode {
			PrivilegeMode::Machine => {
				let status = self.read_csr_raw(CSR_MSTATUS_ADDRESS);
				let mie = (status >> 3) & 1;
				// clear MIE[3], override MPIE[7] with MIE[3], override MPP[12:11] with current privilege encoding
				let new_status = (status & !0x1888) | (mie << 7) | (current_privilege_encoding << 11);
				self.write_csr_raw(CSR_MSTATUS_ADDRESS, new_status);
			},
			PrivilegeMode::Supervisor => {
				let status = self.read_csr_raw(CSR_SSTATUS_ADDRESS);
				let sie = (status >> 1) & 1;
				// clear SIE[1], override SPIE[5] with SIE[1], override SPP[8] with current privilege encoding
				let new_status = (status & !0x122) | (sie << 5) | ((current_privilege_encoding & 1) << 8);
				self.write_csr_raw(CSR_SSTATUS_ADDRESS, new_status);
			},
			PrivilegeMode::User => {
				panic!("Not implemented yet");
			},
			PrivilegeMode::Reserved => panic!() // shouldn't happen
		};
		//println!("Trap! {:x} Clock:{:x}", cause, self.clock);
		true
	}

	fn fetch(&mut self) -> Result<u32, Trap> {
		let word = match self.mmu.fetch_word(self.pc) {
			Ok(word) => word,
			Err(e) => {
				self.pc = self.pc.wrapping_add(4); // @TODO: What if instruction is compressed?
				return Err(e);
			}
		};
		Ok(word)
	}

	fn has_csr_access_privilege(&self, address: u16) -> bool {
		let privilege = (address >> 8) & 0x3; // the lowest privilege level that can access the CSR
		privilege as u8 <= get_privilege_encoding(&self.privilege_mode)
	}

	fn read_csr(&mut self, address: u16) -> Result<u64, Trap> {
		match self.has_csr_access_privilege(address) {
			true => Ok(self.read_csr_raw(address)),
			false => Err(Trap {
				trap_type: TrapType::IllegalInstruction,
				value: self.pc.wrapping_sub(4) // @TODO: Is this always correct?
			})
		}
	}

	fn write_csr(&mut self, address: u16, value: u64) -> Result<(), Trap> {
		match self.has_csr_access_privilege(address) {
			true => {
				/*
				// Checking writability fails some tests so disabling so far
				let read_only = ((address >> 10) & 0x3) == 0x3;
				if read_only {
					return Err(Exception::IllegalInstruction);
				}
				*/
				self.write_csr_raw(address, value);
				if address == CSR_SATP_ADDRESS {
					self.update_addressing_mode(value);
				}
				Ok(())
			},
			false => Err(Trap {
				trap_type: TrapType::IllegalInstruction,
				value: self.pc.wrapping_sub(4) // @TODO: Is this always correct?
			})
		}
	}

	// SSTATUS, SIE, and SIP are subsets of MSTATUS, MIE, and MIP
	fn read_csr_raw(&self, address: u16) -> u64 {
		match address {
			// @TODO: Mask shuld consider of 32-bit mode
			CSR_FFLAGS_ADDRESS => self.csr[CSR_FCSR_ADDRESS as usize] & 0x1f,
			CSR_FRM_ADDRESS => (self.csr[CSR_FCSR_ADDRESS as usize] >> 5) & 0x7,
			// risc-box patch: report mstatus.FS as Dirty (and the SD mirror,
			// bit 63) on every status read. The emulator doesn't track which
			// instructions write f registers, so a Clean FS would make the
			// guest kernel skip saving FP state on context switch while still
			// restoring the (zeroed) save area on switch-in — any process
			// holding live values in f registers across a syscall or
			// preemption gets them wiped (Xorg's spincube rendered every
			// frame from zeroed rotation matrices: an empty image). FS=Dirty
			// always is spec-legal and just costs an unconditional FP
			// save/restore per switch.
			CSR_MSTATUS_ADDRESS =>
				self.csr[CSR_MSTATUS_ADDRESS as usize] | 0x8000000000006000,
			CSR_SSTATUS_ADDRESS =>
				(self.csr[CSR_MSTATUS_ADDRESS as usize] & 0x80000003000de162)
					| 0x8000000000006000,
			CSR_SIE_ADDRESS => self.csr[CSR_MIE_ADDRESS as usize] & 0x222,
			CSR_SIP_ADDRESS => self.csr[CSR_MIP_ADDRESS as usize] & 0x222,
			CSR_TIME_ADDRESS => self.mmu.get_clint().read_mtime(),
			// risc-box patch: cycle counter computed from clock on read; the
			// per-tick write_csr_raw(CSR_CYCLE, clock * 8) in tick() is gone.
			CSR_CYCLE_ADDRESS => self.clock.wrapping_mul(8),
			_ => self.csr[address as usize]
		}
	}

	fn write_csr_raw(&mut self, address: u16, value: u64) {
		// risc-box patch: interrupt delivery is no longer checked after every
		// instruction (see tick), so a write that changes what is pending,
		// what is enabled, or where it would be delivered has to re-arm the
		// check itself. Without this, a guest that unmasks an already-pending
		// interrupt would not take it until the next device service.
		match address {
			CSR_MIP_ADDRESS | CSR_MIE_ADDRESS | CSR_MSTATUS_ADDRESS
			| CSR_SIP_ADDRESS | CSR_SIE_ADDRESS | CSR_SSTATUS_ADDRESS
			| CSR_MIDELEG_ADDRESS => self.check_interrupt = true,
			_ => {}
		}
		match address {
			CSR_FFLAGS_ADDRESS => {
				self.csr[CSR_FCSR_ADDRESS as usize] &= !0x1f;
				self.csr[CSR_FCSR_ADDRESS as usize] |= value & 0x1f;
			},
			CSR_FRM_ADDRESS => {
				self.csr[CSR_FCSR_ADDRESS as usize] &= !0xe0;
				self.csr[CSR_FCSR_ADDRESS as usize] |= (value << 5) & 0xe0;
			},
			CSR_SSTATUS_ADDRESS => {
				self.csr[CSR_MSTATUS_ADDRESS as usize] &= !0x80000003000de162;
				self.csr[CSR_MSTATUS_ADDRESS as usize] |= value & 0x80000003000de162;
				self.mmu.update_mstatus(self.read_csr_raw(CSR_MSTATUS_ADDRESS));
			},
			CSR_SIE_ADDRESS => {
				self.csr[CSR_MIE_ADDRESS as usize] &= !0x222;
				self.csr[CSR_MIE_ADDRESS as usize] |= value & 0x222;
			},
			CSR_SIP_ADDRESS => {
				self.csr[CSR_MIP_ADDRESS as usize] &= !0x222;
				self.csr[CSR_MIP_ADDRESS as usize] |= value & 0x222;
			},
			CSR_MIDELEG_ADDRESS => {
				self.csr[address as usize] = value & 0x666; // from qemu
			},
			CSR_MSTATUS_ADDRESS => {
				self.csr[address as usize] = value;
				self.mmu.update_mstatus(self.read_csr_raw(CSR_MSTATUS_ADDRESS));
			},
			CSR_TIME_ADDRESS => {
				self.mmu.get_mut_clint().write_mtime(value);
			},
			_ => {
				self.csr[address as usize] = value;
			}
		};
	}

	fn _set_fcsr_nv(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x10;
	}

	fn set_fcsr_dz(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x8;
	}

	fn _set_fcsr_of(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x4;
	}

	fn _set_fcsr_uf(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x2;
	}

	fn _set_fcsr_nx(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x1;
	}

	fn update_addressing_mode(&mut self, value: u64) {
		let addressing_mode = match self.xlen {
			Xlen::Bit32 => match value & 0x80000000 {
				0 => AddressingMode::None,
				_ => AddressingMode::SV32
			},
			Xlen::Bit64 => match value >> 60 {
				0 => AddressingMode::None,
				8 => AddressingMode::SV39,
				9 => AddressingMode::SV48,
				_ => {
					println!("Unknown addressing_mode {:x}", value >> 60);
					panic!();
				}
			}
		};
		let ppn = match self.xlen {
			Xlen::Bit32 => value & 0x3fffff,
			Xlen::Bit64 => value & 0xfffffffffff
		};
		self.mmu.update_addressing_mode(addressing_mode);
		self.mmu.update_ppn(ppn);
	}

	// @TODO: Rename to better name?
	fn sign_extend(&self, value: i64) -> i64 {
		match self.xlen {
			Xlen::Bit32 => value as i32 as i64,
			Xlen::Bit64 => value
		}
	}

	// @TODO: Rename to better name?
	fn unsigned_data(&self, value: i64) -> u64 {
		(value as u64) & self.unsigned_data_mask
	}

	// @TODO: Rename to better name?
	fn most_negative(&self) -> i64 {
		match self.xlen {
			Xlen::Bit32 => std::i32::MIN as i64,
			Xlen::Bit64 => std::i64::MIN
		}
	}

	// @TODO: Optimize
	fn uncompress(&self, halfword: u32) -> u32 {
		let op = halfword & 0x3; // [1:0]
		let funct3 = (halfword >> 13) & 0x7; // [15:13]

		match op {
			0 => match funct3 {
				0 => {
					// C.ADDI4SPN
					// addi rd+8, x2, nzuimm
					let rd = (halfword >> 2) & 0x7; // [4:2]
					let nzuimm =
						((halfword >> 7) & 0x30) | // nzuimm[5:4] <= [12:11]
						((halfword >> 1) & 0x3c0) | // nzuimm{9:6] <= [10:7]
						((halfword >> 4) & 0x4) | // nzuimm[2] <= [6]
						((halfword >> 2) & 0x8); // nzuimm[3] <= [5]
					// nzuimm == 0 is reserved instruction
					if nzuimm != 0 {
						return (nzuimm << 20) | (2 << 15) | ((rd + 8) << 7) | 0x13;
					}
				},
				1 => {
					// @TODO: Support C.LQ for 128-bit
					// C.FLD for 32, 64-bit
					// fld rd+8, offset(rs1+8)
					let rd = (halfword >> 2) & 0x7; // [4:2]
					let rs1 = (halfword >> 7) & 0x7; // [9:7]
					let offset =
						((halfword >> 7) & 0x38) | // offset[5:3] <= [12:10]
						((halfword << 1) & 0xc0); // offset[7:6] <= [6:5]
					return (offset << 20) | ((rs1 + 8) << 15) | (3 << 12) | ((rd + 8) << 7) | 0x7;
				},
				2 => {
					// C.LW
					// lw rd+8, offset(rs1+8)
					let rs1 = (halfword >> 7) & 0x7; // [9:7]
					let rd = (halfword >> 2) & 0x7; // [4:2]
					let offset =
						((halfword >> 7) & 0x38) | // offset[5:3] <= [12:10]
						((halfword >> 4) & 0x4) | // offset[2] <= [6]
						((halfword << 1) & 0x40); // offset[6] <= [5]
					return (offset << 20) | ((rs1 + 8) << 15) | (2 << 12) | ((rd + 8) << 7) | 0x3;
				},
				3 => {
					// @TODO: Support C.FLW in 32-bit mode
					// C.LD in 64-bit mode
					// ld rd+8, offset(rs1+8)
					let rs1 = (halfword >> 7) & 0x7; // [9:7]
					let rd = (halfword >> 2) & 0x7; // [4:2]
					let offset =
						((halfword >> 7) & 0x38) | // offset[5:3] <= [12:10]
						((halfword << 1) & 0xc0); // offset[7:6] <= [6:5]
					return (offset << 20) | ((rs1 + 8) << 15) | (3 << 12) | ((rd + 8) << 7) | 0x3;
				},
				4 => {
					// Reserved
				},
				5 => {
					// C.FSD
					// fsd rs2+8, offset(rs1+8)
					let rs1 = (halfword >> 7) & 0x7; // [9:7]
					let rs2 = (halfword >> 2) & 0x7; // [4:2]
					let offset = 
						((halfword >> 7) & 0x38) | // uimm[5:3] <= [12:10]
						((halfword << 1) & 0xc0); // uimm[7:6] <= [6:5]
					let imm11_5 = (offset >> 5) & 0x7f;
					let imm4_0 = offset & 0x1f;
					return (imm11_5 << 25) | ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | (3 << 12) | (imm4_0 << 7) | 0x27;
				},
				6 => {
					// C.SW
					// sw rs2+8, offset(rs1+8)
					let rs1 = (halfword >> 7) & 0x7; // [9:7]
					let rs2 = (halfword >> 2) & 0x7; // [4:2]
					let offset = 
						((halfword >> 7) & 0x38) | // offset[5:3] <= [12:10]
						((halfword << 1) & 0x40) | // offset[6] <= [5]
						((halfword >> 4) & 0x4); // offset[2] <= [6]
					let imm11_5 = (offset >> 5) & 0x7f;
					let imm4_0 = offset & 0x1f;
					return (imm11_5 << 25) | ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | (2 << 12) | (imm4_0 << 7) | 0x23;
				},
				7 => {
					// @TODO: Support C.FSW in 32-bit mode
					// C.SD
					// sd rs2+8, offset(rs1+8)
					let rs1 = (halfword >> 7) & 0x7; // [9:7]
					let rs2 = (halfword >> 2) & 0x7; // [4:2]
					let offset = 
						((halfword >> 7) & 0x38) | // uimm[5:3] <= [12:10]
						((halfword << 1) & 0xc0); // uimm[7:6] <= [6:5]
					let imm11_5 = (offset >> 5) & 0x7f;
					let imm4_0 = offset & 0x1f;
					return (imm11_5 << 25) | ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | (3 << 12) | (imm4_0 << 7) | 0x23;
				},
				_ => {} // Not happens
			},
			1 => {
				match funct3 {
					0 => {
						let r = (halfword >> 7) & 0x1f; // [11:7]
						let imm = match halfword & 0x1000 {
							0x1000 => 0xffffffc0,
							_ => 0
						} | // imm[31:6] <= [12]
						((halfword >> 7) & 0x20) | // imm[5] <= [12]
						((halfword >> 2) & 0x1f); // imm[4:0] <= [6:2]
						// C.ADDI (r=0,imm=0 is C.NOP; r=0,imm!=0 is a HINT -- hints
						// execute as their expansion; x0 discards the write anyway)
						// addi r, r, imm
						return (imm << 20) | (r << 15) | (r << 7) | 0x13;
					},
					1 => {
						// @TODO: Support C.JAL in 32-bit mode
						// C.ADDIW
						// addiw r, r, imm
						let r = (halfword >> 7) & 0x1f;
						let imm = match halfword & 0x1000 {
							0x1000 => 0xffffffc0,
							_ => 0
						} | // imm[31:6] <= [12]
						((halfword >> 7) & 0x20) | // imm[5] <= [12]
						((halfword >> 2) & 0x1f); // imm[4:0] <= [6:2]
						if r != 0 {
							return (imm << 20) | (r << 15) | (r << 7) | 0x1b;
						}
						// r == 0 is reserved instruction
					},
					2 => {
						// C.LI
						// addi rd, x0, imm
						let r = (halfword >> 7) & 0x1f;
						let imm = match halfword & 0x1000 {
							0x1000 => 0xffffffc0,
							_ => 0
						} | // imm[31:6] <= [12]
						((halfword >> 7) & 0x20) | // imm[5] <= [12]
						((halfword >> 2) & 0x1f); // imm[4:0] <= [6:2]
						// r == 0 is a HINT; addi x0, x0, imm is a no-op, emit it anyway
						return (imm << 20) | (r << 7) | 0x13;
					},
					3 => {
						let r = (halfword >> 7) & 0x1f; // [11:7]
						if r == 2 {
							// C.ADDI16SP
							// addi r, r, nzimm
							let imm = match halfword & 0x1000 {
								0x1000 => 0xfffffc00,
								_ => 0
							} | // imm[31:10] <= [12]
							((halfword >> 3) & 0x200) | // imm[9] <= [12]
							((halfword >> 2) & 0x10) | // imm[4] <= [6]
							((halfword << 1) & 0x40) | // imm[6] <= [5]
							((halfword << 4) & 0x180) | // imm[8:7] <= [4:3]
							((halfword << 3) & 0x20); // imm[5] <= [2]
							if imm != 0 {
								return (imm << 20) | (r << 15) | (r << 7) | 0x13;
							}
							// imm == 0 is for reserved instruction
						}
						if r != 2 { // r == 0 is a HINT; lui x0 is a no-op
							// C.LUI
							// lui r, nzimm
							let nzimm = match halfword & 0x1000 {
								0x1000 => 0xfffc0000,
								_ => 0
							} | // nzimm[31:18] <= [12]
							((halfword << 5) & 0x20000) | // nzimm[17] <= [12]
							((halfword << 10) & 0x1f000); // nzimm[16:12] <= [6:2]
							if nzimm != 0 {
								return nzimm | (r << 7) | 0x37;
							}
							// nzimm == 0 is for reserved instruction
						}
					},
					4 => {
						let funct2 = (halfword >> 10) & 0x3; // [11:10]
						match funct2 {
							0 => {
								// C.SRLI
								// c.srli rs1+8, rs1+8, shamt
								let shamt = 
									((halfword >> 7) & 0x20) | // shamt[5] <= [12]
									((halfword >> 2) & 0x1f); // shamt[4:0] <= [6:2]
								let rs1 = (halfword >> 7) & 0x7; // [9:7]
								return (shamt << 20) | ((rs1 + 8) << 15) | (5 << 12) | ((rs1 + 8) << 7) | 0x13;
							},
							1 => {
								// C.SRAI
								// srai rs1+8, rs1+8, shamt
								let shamt = 
									((halfword >> 7) & 0x20) | // shamt[5] <= [12]
									((halfword >> 2) & 0x1f); // shamt[4:0] <= [6:2]
								let rs1 = (halfword >> 7) & 0x7; // [9:7]
								return (0x20 << 25) | (shamt << 20) | ((rs1 + 8) << 15) | (5 << 12) | ((rs1 + 8) << 7) | 0x13;
							},
							2 => {
								// C.ANDI
								// andi, r+8, r+8, imm
								let r = (halfword >> 7) & 0x7; // [9:7]
								let imm = match halfword & 0x1000 {
									0x1000 => 0xffffffc0,
									_ => 0
								} | // imm[31:6] <= [12]
								((halfword >> 7) & 0x20) | // imm[5] <= [12]
								((halfword >> 2) & 0x1f); // imm[4:0] <= [6:2]
								return (imm << 20) | ((r + 8) << 15) | (7 << 12) | ((r + 8) << 7) | 0x13;
							},
							3 => {
								let funct1 = (halfword >> 12) & 1; // [12]
								let funct2_2 = (halfword >> 5) & 0x3; // [6:5]
								let rs1 = (halfword >> 7) & 0x7;
								let rs2 = (halfword >> 2) & 0x7;
								match funct1 {
									0 => match funct2_2 {
										0 => {
											// C.SUB
											// sub rs1+8, rs1+8, rs2+8
											return (0x20 << 25) | ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | ((rs1 + 8) << 7) | 0x33;
										},
										1 => {
											// C.XOR
											// xor rs1+8, rs1+8, rs2+8
											return ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | (4 << 12) | ((rs1 + 8) << 7) | 0x33;
										},
										2 => {
											// C.OR
											// or rs1+8, rs1+8, rs2+8
											return ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | (6 << 12) | ((rs1 + 8) << 7) | 0x33;
										},
										3 => {
											// C.AND
											// and rs1+8, rs1+8, rs2+8
											return ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | (7 << 12) | ((rs1 + 8) << 7) | 0x33;
										},
										_ => {} // Not happens
									},
									1 => match funct2_2 {
										0 => {
											// C.SUBW
											// subw r1+8, r1+8, r2+8
											return (0x20 << 25) | ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | ((rs1 + 8) << 7) | 0x3b;
										},
										1 => {
											// C.ADDW
											// addw r1+8, r1+8, r2+8
											return ((rs2 + 8) << 20) | ((rs1 + 8) << 15) | ((rs1 + 8) << 7) | 0x3b;
										},
										2 => {
											// Reserved
										},
										3 => {
											// Reserved
										},
										_ => {} // Not happens
									},
									_ => {} // No happens
								};
							},
							_ => {} // not happens
						};
					},
					5 => {
						// C.J
						// jal x0, imm
						let offset =
							match halfword & 0x1000 {
								0x1000 => 0xfffff000,
								_ => 0
							} | // offset[31:12] <= [12]
							((halfword >> 1) & 0x800) | // offset[11] <= [12]
							((halfword >> 7) & 0x10) | // offset[4] <= [11]
							((halfword >> 1) & 0x300) | // offset[9:8] <= [10:9]
							((halfword << 2) & 0x400) | // offset[10] <= [8]
							((halfword >> 1) & 0x40) | // offset[6] <= [7]
							((halfword << 1) & 0x80) | // offset[7] <= [6]
							((halfword >> 2) & 0xe) | // offset[3:1] <= [5:3]
							((halfword << 3) & 0x20); // offset[5] <= [2]
						let imm =
							((offset >> 1) & 0x80000) | // imm[19] <= offset[20]
							((offset << 8) & 0x7fe00) | // imm[18:9] <= offset[10:1]
							((offset >> 3) & 0x100) | // imm[8] <= offset[11]
							((offset >> 12) & 0xff); // imm[7:0] <= offset[19:12]
						return (imm << 12) | 0x6f;
					},
					6 => {
						// C.BEQZ
						// beq r+8, x0, offset
						let r = (halfword >> 7) & 0x7;
						let offset =
							match halfword & 0x1000 {
								0x1000 => 0xfffffe00,
								_ => 0
							} | // offset[31:9] <= [12]
							((halfword >> 4) & 0x100) | // offset[8] <= [12]
							((halfword >> 7) & 0x18) | // offset[4:3] <= [11:10]
							((halfword << 1) & 0xc0) | // offset[7:6] <= [6:5]
							((halfword >> 2) & 0x6) | // offset[2:1] <= [4:3]
							((halfword << 3) & 0x20); // offset[5] <= [2]
						let imm2 =
							((offset >> 6) & 0x40) | // imm2[6] <= [12]
							((offset >> 5) & 0x3f); // imm2[5:0] <= [10:5]
						let imm1 =
							(offset & 0x1e) | // imm1[4:1] <= [4:1]
							((offset >> 11) & 0x1); // imm1[0] <= [11]
						return (imm2 << 25) | ((r + 8) << 15) | (imm1 << 7) | 0x63; // beq r+8, x0 (canonical operand order)
					},
					7 => {
						// C.BNEZ
						// bne r+8, x0, offset
						let r = (halfword >> 7) & 0x7;
						let offset =
							match halfword & 0x1000 {
								0x1000 => 0xfffffe00,
								_ => 0
							} | // offset[31:9] <= [12]
							((halfword >> 4) & 0x100) | // offset[8] <= [12]
							((halfword >> 7) & 0x18) | // offset[4:3] <= [11:10]
							((halfword << 1) & 0xc0) | // offset[7:6] <= [6:5]
							((halfword >> 2) & 0x6) | // offset[2:1] <= [4:3]
							((halfword << 3) & 0x20); // offset[5] <= [2]
						let imm2 =
							((offset >> 6) & 0x40) | // imm2[6] <= [12]
							((offset >> 5) & 0x3f); // imm2[5:0] <= [10:5]
						let imm1 =
							(offset & 0x1e) | // imm1[4:1] <= [4:1]
							((offset >> 11) & 0x1); // imm1[0] <= [11]
						return (imm2 << 25) | ((r + 8) << 15) | (1 << 12) | (imm1 << 7) | 0x63; // bne r+8, x0 (canonical operand order)
					},
					_ => {} // No happens
				};
			},
			2 => {
				match funct3 {
					0 => {
						// C.SLLI
						// slli r, r, shamt
						let r = (halfword >> 7) & 0x1f;
						let shamt =
							((halfword >> 7) & 0x20) | // imm[5] <= [12]
							((halfword >> 2) & 0x1f); // imm[4:0] <= [6:2]
						// r == 0 (and shamt == 0) are HINTs; slli x0 is a no-op
						return (shamt << 20) | (r << 15) | (1 << 12) | (r << 7) | 0x13;
					},
					1 => {
						// C.FLDSP
						// fld rd, offset(x2)
						let rd = (halfword >> 7) & 0x1f;
						let offset =
							((halfword >> 7) & 0x20) | // offset[5] <= [12]
							((halfword >> 2) & 0x18) | // offset[4:3] <= [6:5]
							((halfword << 4) & 0x1c0); // offset[8:6] <= [4:2]
						// rd is a FLOAT register here, so rd == 0 means f0, which is valid
						// (unlike x0 for the integer LWSP/LDSP forms). gcc emits
						// `c.fldsp f0, off(sp)` for FP spill reloads; gating on rd != 0
						// wrongly raised SIGILL in Xorg's pixman/fb render path.
						return (offset << 20) | (2 << 15) | (3 << 12) | (rd << 7) | 0x7;
					},
					2 => {
						// C.LWSP
						// lw r, offset(x2)
						let r = (halfword >> 7) & 0x1f;
						let offset =
							((halfword >> 7) & 0x20) | // offset[5] <= [12]
							((halfword >> 2) & 0x1c) | // offset[4:2] <= [6:4]
							((halfword << 4) & 0xc0); // offset[7:6] <= [3:2]
						if r != 0 {
							return (offset << 20) | (2 << 15) | (2 << 12) | (r << 7) | 0x3;
						}
						// r == 0 is reseved instruction
					},
					3 => {
						// @TODO: Support C.FLWSP in 32-bit mode
						// C.LDSP
						// ld rd, offset(x2)
						let rd = (halfword >> 7) & 0x1f;
						let offset =
							((halfword >> 7) & 0x20) | // offset[5] <= [12]
							((halfword >> 2) & 0x18) | // offset[4:3] <= [6:5]
							((halfword << 4) & 0x1c0); // offset[8:6] <= [4:2]
						if rd != 0 {
							return (offset << 20) | (2 << 15) | (3 << 12) | (rd << 7) | 0x3;
						}
						// rd == 0 is reseved instruction
					},
					4 => {
						let funct1 = (halfword >> 12) & 1; // [12]
						let rs1 = (halfword >> 7) & 0x1f; // [11:7]
						let rs2 = (halfword >> 2) & 0x1f; // [6:2]
						match funct1 {
							0 => {
								if rs1 != 0 && rs2 == 0 {
									// C.JR
									// jalr x0, 0(rs1)
									return (rs1 << 15) | 0x67;
								}
								// rs1 == 0 is reserved instruction
								if rs2 != 0 {
									// C.MV (rd == 0 is a HINT; add x0 is a no-op)
									// add rd, x0, rs2
									return (rs2 << 20) | (rs1 << 7) | 0x33;
								}
							},
							1 => {
								if rs1 == 0 && rs2 == 0 {
									// C.EBREAK
									// ebreak
									return 0x00100073;
								}
								if rs1 != 0 && rs2 == 0 {
									// C.JALR
									// jalr x1, 0(rs1)
									return (rs1 << 15) | (1 << 7) | 0x67;
								}
								if rs2 != 0 {
									// C.ADD (rd == 0 is a HINT; add x0 is a no-op)
									// add rd, rd, rs2
									return (rs2 << 20) | (rs1 << 15) | (rs1 << 7) | 0x33;
								}
							},
							_ => {} // Not happens
						};
					},
					5 => {
						// @TODO: Implement
						// C.FSDSP
						// fsd rs2, offset(x2)
						let rs2 = (halfword >> 2) & 0x1f; // [6:2]
						let offset =
							((halfword >> 7) & 0x38) | // offset[5:3] <= [12:10]
							((halfword >> 1) & 0x1c0); // offset[8:6] <= [9:7]
						let imm11_5 = (offset >> 5) & 0x3f;
						let imm4_0 = offset & 0x1f;
						return (imm11_5 << 25) | (rs2 << 20) | (2 << 15) | (3 << 12) | (imm4_0 << 7) | 0x27;
					},
					6 => {
						// C.SWSP
						// sw rs2, offset(x2)
						let rs2 = (halfword >> 2) & 0x1f; // [6:2]
						let offset =
							((halfword >> 7) & 0x3c) | // offset[5:2] <= [12:9]
							((halfword >> 1) & 0xc0); // offset[7:6] <= [8:7]
						let imm11_5 = (offset >> 5) & 0x3f;
						let imm4_0 = offset & 0x1f;
						return (imm11_5 << 25) | (rs2 << 20) | (2 << 15) | (2 << 12) | (imm4_0 << 7) | 0x23;
					},
					7 => {
						// @TODO: Support C.FSWSP in 32-bit mode
						// C.SDSP
						// sd rs, offset(x2)
						let rs2 = (halfword >> 2) & 0x1f; // [6:2]
						let offset =
							((halfword >> 7) & 0x38) | // offset[5:3] <= [12:10]
							((halfword >> 1) & 0x1c0); // offset[8:6] <= [9:7]
						let imm11_5 = (offset >> 5) & 0x3f;
						let imm4_0 = offset & 0x1f;
						return (imm11_5 << 25) | (rs2 << 20) | (2 << 15) | (3 << 12) | (imm4_0 << 7) | 0x23;
					},
					_ => {} // Not happens
				};
			},
			_ => {} // No happnes
		};
		0xffffffff // Return invalid value
	}

	/// Disassembles an instruction pointed by Program Counter.
	pub fn disassemble_next_instruction(&mut self) -> String {
		// @TODO: Fetching can make a side effect,
		// for example updating page table entry or update peripheral hardware registers.
		// But ideally disassembling doesn't want to cause any side effect.
		// How can we avoid side effect?
		let mut original_word = match self.mmu.fetch_word(self.pc) {
			Ok(data) => data,
			Err(_e) => {
				return format!("PC:{:016x}, InstructionPageFault Trap!\n", self.pc);
			}
		};

		let word = match (original_word & 0x3) == 0x3 {
			true => original_word,
			false => {
				original_word &= 0xffff;
				self.uncompress(original_word)
			}
		};

		let inst = {match self.decode_raw(word) {
			Ok(inst) => inst,
			Err(()) => {
				return format!("Unknown instruction PC:{:x} WORD:{:x}", self.pc, original_word);
			}
		}};

		let mut s = format!("PC:{:016x} ", self.unsigned_data(self.pc as i64));
		s += &format!("{:08x} ", original_word);
		s += &format!("{} ", inst.name);
		s += &format!("{}", (inst.disassemble)(self, word, self.pc, true));
		s
	}

	/// Returns mutable `Mmu`
	pub fn get_mut_mmu(&mut self) -> &mut Mmu {
		&mut self.mmu
	}

	/// Returns `Mmu` (risc-box patch: the immutable side of the pair — the
	/// host's framebuffer scanout reads DRAM without touching CPU state)
	pub fn get_mmu(&self) -> &Mmu {
		&self.mmu
	}

	/// Returns mutable `Terminal`
	pub fn get_mut_terminal(&mut self) -> &mut Box<dyn Terminal> {
		self.mmu.get_mut_uart().get_mut_terminal()
	}
}

struct Instruction {
	mask: u32,
	data: u32, // @TODO: rename
	name: &'static str,
	operation: fn(cpu: &mut Cpu, word: u32, address: u64) -> Result<(), Trap>,
	disassemble: fn(cpu: &mut Cpu, word: u32, address: u64, evaluate: bool) -> String
}

struct FormatB {
	rs1: usize,
	rs2: usize,
	imm: u64
}

fn parse_format_b(word: u32) -> FormatB {
	FormatB {
		rs1: ((word >> 15) & 0x1f) as usize, // [19:15]
		rs2: ((word >> 20) & 0x1f) as usize, // [24:20]
		imm: (
			match word & 0x80000000 { // imm[31:12] = [31]
				0x80000000 => 0xfffff000,
				_ => 0
			} |
			((word << 4) & 0x00000800) | // imm[11] = [7]
			((word >> 20) & 0x000007e0) | // imm[10:5] = [30:25]
			((word >> 7) & 0x0000001e) // imm[4:1] = [11:8]
		) as i32 as i64 as u64
	}
}

fn dump_format_b(cpu: &mut Cpu, word: u32, address: u64, evaluate: bool) -> String {
	let f = parse_format_b(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rs1));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs1]);
	}
	s += &format!(",{}", get_register_name(f.rs2));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs2]);
	}
	s += &format!(",{:x}", address.wrapping_add(f.imm));
	s
}

struct FormatCSR {
	csr: u16,
	rs: usize,
	rd: usize
}

fn parse_format_csr(word: u32) -> FormatCSR {
	FormatCSR {
		csr: ((word >> 20) & 0xfff) as u16, // [31:20]
		rs: ((word >> 15) & 0x1f) as usize, // [19:15], also uimm
		rd: ((word >> 7) & 0x1f) as usize // [11:7]
	}
}

fn dump_format_csr(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_csr(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	// @TODO: Use CSR name
	s += &format!(",{:x}", f.csr);
	if evaluate {
		s += &format!(":{:x}", cpu.read_csr_raw(f.csr));
	}
	s += &format!(",{}", get_register_name(f.rs));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs]);
	}
	s
}

struct FormatI {
	rd: usize,
	rs1: usize,
	imm: i64
}

fn parse_format_i(word: u32) -> FormatI {
	FormatI {
		rd: ((word >> 7) & 0x1f) as usize, // [11:7]
		rs1: ((word >> 15) & 0x1f) as usize, // [19:15]
		imm: (
			match word & 0x80000000 { // imm[31:11] = [31]
				0x80000000 => 0xfffff800,
				_ => 0
			} |
			((word >> 20) & 0x000007ff) // imm[10:0] = [30:20]
		) as i32 as i64
	}
}

fn dump_format_i(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_i(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	s += &format!(",{}", get_register_name(f.rs1));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs1]);
	}
	s += &format!(",{:x}", f.imm);
	s
}

fn dump_format_i_mem(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_i(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	s += &format!(",{:x}({}", f.imm, get_register_name(f.rs1));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs1]);
	}
	s += &format!(")");
	s
}

struct FormatJ {
	rd: usize,
	imm: u64
}

fn parse_format_j(word: u32) -> FormatJ {
	FormatJ {
		rd: ((word >> 7) & 0x1f) as usize, // [11:7]
		imm: (
			match word & 0x80000000 { // imm[31:20] = [31]
				0x80000000 => 0xfff00000,
				_ => 0
			} |
			(word & 0x000ff000) | // imm[19:12] = [19:12]
			((word & 0x00100000) >> 9) | // imm[11] = [20]
			((word & 0x7fe00000) >> 20) // imm[10:1] = [30:21]
		) as i32 as i64 as u64
	}
}

fn dump_format_j(cpu: &mut Cpu, word: u32, address: u64, evaluate: bool) -> String {
	let f = parse_format_j(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	s += &format!(",{:x}", address.wrapping_add(f.imm));
	s
}

struct FormatR {
	rd: usize,
	rs1: usize,
	rs2: usize
}

fn parse_format_r(word: u32) -> FormatR {
	FormatR {
		rd: ((word >> 7) & 0x1f) as usize, // [11:7]
		rs1: ((word >> 15) & 0x1f) as usize, // [19:15]
		rs2: ((word >> 20) & 0x1f) as usize // [24:20]
	}
}

fn dump_format_r(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_r(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	s += &format!(",{}", get_register_name(f.rs1));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs1]);
	}
	s += &format!(",{}", get_register_name(f.rs2));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs2]);
	}
	s
}

// has rs3
struct FormatR2 {
	rd: usize,
	rs1: usize,
	rs2: usize,
	rs3: usize
}

fn parse_format_r2(word: u32) -> FormatR2 {
	FormatR2 {
		rd: ((word >> 7) & 0x1f) as usize, // [11:7]
		rs1: ((word >> 15) & 0x1f) as usize, // [19:15]
		rs2: ((word >> 20) & 0x1f) as usize, // [24:20]
		rs3: ((word >> 27) & 0x1f) as usize // [31:27]
	}
}

fn dump_format_r2(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_r2(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	s += &format!(",{}", get_register_name(f.rs1));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs1]);
	}
	s += &format!(",{}", get_register_name(f.rs2));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs2]);
	}
	s += &format!(",{}", get_register_name(f.rs3));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs3]);
	}
	s
}

struct FormatS {
	rs1: usize,
	rs2: usize,
	imm: i64
}

fn parse_format_s(word: u32) -> FormatS {
	FormatS {
		rs1: ((word >> 15) & 0x1f) as usize, // [19:15]
		rs2: ((word >> 20) & 0x1f) as usize, // [24:20]
		imm: (
			match word & 0x80000000 {
				0x80000000 => 0xfffff000,
				_ => 0
			} | // imm[31:12] = [31]
			((word >> 20) & 0xfe0) | // imm[11:5] = [31:25]
			((word >> 7) & 0x1f) // imm[4:0] = [11:7]
		) as i32 as i64
	}
}

fn dump_format_s(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_s(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rs2));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs2]);
	}
	s += &format!(",{:x}({}", f.imm, get_register_name(f.rs1));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rs1]);
	}
	s += &format!(")");
	s
}

struct FormatU {
	rd: usize,
	imm: u64
}

fn parse_format_u(word: u32) -> FormatU {
	FormatU {
		rd: ((word >> 7) & 0x1f) as usize, // [11:7]
		imm: (
			match word & 0x80000000 {
				0x80000000 => 0xffffffff00000000,
				_ => 0
			} | // imm[63:32] = [31]
			((word as u64) & 0xfffff000) // imm[31:12] = [31:12]
		) as u64
	}
}

fn dump_format_u(cpu: &mut Cpu, word: u32, _address: u64, evaluate: bool) -> String {
	let f = parse_format_u(word);
	let mut s = String::new();
	s += &format!("{}", get_register_name(f.rd));
	if evaluate {
		s += &format!(":{:x}", cpu.x[f.rd]);
	}
	s += &format!(",{:x}", f.imm);
	s
}

fn dump_empty(_cpu: &mut Cpu, _word: u32, _address: u64, _evaluate: bool) -> String {
	String::new()
}

fn get_register_name(num: usize) -> &'static str {
	match num {
		0 => "zero",
		1 => "ra",
		2 => "sp",
		3 => "gp",
		4 => "tp",
		5 => "t0",
		6 => "t1",
		7 => "t2",
		8 => "s0",
		9 => "s1",
		10 => "a0",
		11 => "a1",
		12 => "a2",
		13 => "a3",
		14 => "a4",
		15 => "a5",
		16 => "a6",
		17 => "a7",
		18 => "s2",
		19 => "s3",
		20 => "s4",
		21 => "s5",
		22 => "s6",
		23 => "s7",
		24 => "s8",
		25 => "s9",
		26 => "s10",
		27 => "s11",
		28 => "t3",
		29 => "t4",
		30 => "t5",
		31 => "t6",
		_ => panic!("Unknown register num {}", num)
	}
}

const INSTRUCTION_NUM: usize = 153;

// @TODO: Reorder in often used order as 
const INSTRUCTIONS: [Instruction; INSTRUCTION_NUM] = [
	Instruction {
		mask: 0xfe00707f,
		data: 0x00000033,
		name: "ADD",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_add(cpu.x[f.rs2]));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00000013,
		name: "ADDI",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_add(f.imm));
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x0000001b,
		name: "ADDIW",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = cpu.x[f.rs1].wrapping_add(f.imm) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0000003b,
		name: "ADDW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.x[f.rs1].wrapping_add(cpu.x[f.rs2]) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x0000302f,
		name: "AMOADD.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, cpu.x[f.rs2].wrapping_add(tmp) as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x0000202f,
		name: "AMOADD.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32 as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, cpu.x[f.rs2].wrapping_add(tmp) as u32) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x6000302f,
		name: "AMOAND.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, (cpu.x[f.rs2] & tmp) as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x6000202f,
		name: "AMOAND.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32 as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, (cpu.x[f.rs2] & tmp) as u32) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0xe000302f,
		name: "AMOMAXU.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data,
				Err(e) => return Err(e)
			};
			let max = match cpu.x[f.rs2] as u64 >= tmp {
				true => cpu.x[f.rs2] as u64,
				false => tmp
			};
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, max) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0xe000202f,
		name: "AMOMAXU.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data,
				Err(e) => return Err(e)
			};
			let max = match cpu.x[f.rs2] as u32 >= tmp {
				true => cpu.x[f.rs2] as u32,
				false => tmp
			};
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, max) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x4000302f,
		name: "AMOOR.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, (cpu.x[f.rs2] | tmp) as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x4000202f,
		name: "AMOOR.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32 as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, (cpu.x[f.rs2] | tmp) as u32) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x0800302f,
		name: "AMOSWAP.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, cpu.x[f.rs2] as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x0800202f,
		name: "AMOSWAP.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32 as i64,
				Err(e) => return Err(e)
			};
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, cpu.x[f.rs2] as u32) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00007033,
		name: "AND",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] & cpu.x[f.rs2]);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00007013,
		name: "ANDI",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] & f.imm);
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0x0000007f,
		data: 0x00000017,
		name: "AUIPC",
		operation: |cpu, word, address| {
			let f = parse_format_u(word);
			cpu.x[f.rd] = cpu.sign_extend(address.wrapping_add(f.imm) as i64);
			Ok(())
		},
		disassemble: dump_format_u
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00000063,
		name: "BEQ",
		operation: |cpu, word, address| {
			let f = parse_format_b(word);
			if cpu.sign_extend(cpu.x[f.rs1]) == cpu.sign_extend(cpu.x[f.rs2]) {
				cpu.pc = address.wrapping_add(f.imm);
			}
			Ok(())
		},
		disassemble: dump_format_b
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00005063,
		name: "BGE",
		operation: |cpu, word, address| {
			let f = parse_format_b(word);
			if cpu.sign_extend(cpu.x[f.rs1]) >= cpu.sign_extend(cpu.x[f.rs2]) {
				cpu.pc = address.wrapping_add(f.imm);
			}
			Ok(())
		},
		disassemble: dump_format_b
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00007063,
		name: "BGEU",
		operation: |cpu, word, address| {
			let f = parse_format_b(word);
			if cpu.unsigned_data(cpu.x[f.rs1]) >= cpu.unsigned_data(cpu.x[f.rs2]) {
				cpu.pc = address.wrapping_add(f.imm);
			}
			Ok(())
		},
		disassemble: dump_format_b
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00004063,
		name: "BLT",
		operation: |cpu, word, address| {
			let f = parse_format_b(word);
			if cpu.sign_extend(cpu.x[f.rs1]) < cpu.sign_extend(cpu.x[f.rs2]) {
				cpu.pc = address.wrapping_add(f.imm);
			}
			Ok(())
		},
		disassemble: dump_format_b
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00006063,
		name: "BLTU",
		operation: |cpu, word, address| {
			let f = parse_format_b(word);
			if cpu.unsigned_data(cpu.x[f.rs1]) < cpu.unsigned_data(cpu.x[f.rs2]) {
				cpu.pc = address.wrapping_add(f.imm);
			}
			Ok(())
		},
		disassemble: dump_format_b
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00001063,
		name: "BNE",
		operation: |cpu, word, address| {
			let f = parse_format_b(word);
			if cpu.sign_extend(cpu.x[f.rs1]) != cpu.sign_extend(cpu.x[f.rs2]) {
				cpu.pc = address.wrapping_add(f.imm);
			}
			Ok(())
		},
		disassemble: dump_format_b
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00003073,
		name: "CSRRC",
		operation: |cpu, word, _address| {
			let f = parse_format_csr(word);
			let data = match cpu.read_csr(f.csr) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let tmp = cpu.x[f.rs];
			cpu.x[f.rd] = cpu.sign_extend(data);
			match cpu.write_csr(f.csr, (cpu.x[f.rd] & !tmp) as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_csr
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00007073,
		name: "CSRRCI",
		operation: |cpu, word, _address| {
			let f = parse_format_csr(word);
			let data = match cpu.read_csr(f.csr) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = cpu.sign_extend(data);
			match cpu.write_csr(f.csr, (cpu.x[f.rd] & !(f.rs as i64)) as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_csr
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00002073,
		name: "CSRRS",
		operation: |cpu, word, _address| {
			let f = parse_format_csr(word);
			let data = match cpu.read_csr(f.csr) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let tmp = cpu.x[f.rs];
			cpu.x[f.rd] = cpu.sign_extend(data);
			match cpu.write_csr(f.csr, cpu.unsigned_data(cpu.x[f.rd] | tmp)) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_csr
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00006073,
		name: "CSRRSI",
		operation: |cpu, word, _address| {
			let f = parse_format_csr(word);
			let data = match cpu.read_csr(f.csr) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = cpu.sign_extend(data);
			match cpu.write_csr(f.csr, cpu.unsigned_data(cpu.x[f.rd] | (f.rs as i64))) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_csr
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00001073,
		name: "CSRRW",
		operation: |cpu, word, _address| {
			let f = parse_format_csr(word);
			let data = match cpu.read_csr(f.csr) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let tmp = cpu.x[f.rs];
			cpu.x[f.rd] = cpu.sign_extend(data);
			match cpu.write_csr(f.csr, cpu.unsigned_data(tmp)) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_csr
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00005073,
		name: "CSRRWI",
		operation: |cpu, word, _address| {
			let f = parse_format_csr(word);
			let data = match cpu.read_csr(f.csr) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = cpu.sign_extend(data);
			match cpu.write_csr(f.csr, f.rs as u64) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_csr
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02004033,
		name: "DIV",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.x[f.rs1];
			let divisor = cpu.x[f.rs2];
			if divisor == 0 {
				cpu.x[f.rd] = -1;
			} else if dividend == cpu.most_negative() && divisor == -1 {
				cpu.x[f.rd] = dividend;
			} else {
				cpu.x[f.rd] = cpu.sign_extend(dividend.wrapping_div(divisor))
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02005033,
		name: "DIVU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.unsigned_data(cpu.x[f.rs1]);
			let divisor = cpu.unsigned_data(cpu.x[f.rs2]);
			if divisor == 0 {
				cpu.x[f.rd] = -1;
			} else {
				cpu.x[f.rd] = cpu.sign_extend(dividend.wrapping_div(divisor) as i64)
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0200503b,
		name: "DIVUW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.unsigned_data(cpu.x[f.rs1]) as u32;
			let divisor = cpu.unsigned_data(cpu.x[f.rs2]) as u32;
			if divisor == 0 {
				cpu.x[f.rd] = -1;
			} else {
				cpu.x[f.rd] = dividend.wrapping_div(divisor) as i32 as i64
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0200403b,
		name: "DIVW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.x[f.rs1] as i32;
			let divisor = cpu.x[f.rs2] as i32;
			if divisor == 0 {
				cpu.x[f.rd] = -1;
			} else if dividend == std::i32::MIN && divisor == -1 {
				cpu.x[f.rd] = dividend as i32 as i64;
			} else {
				cpu.x[f.rd] = dividend.wrapping_div(divisor) as i32 as i64
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xffffffff,
		data: 0x00100073,
		name: "EBREAK",
		operation: |_cpu, _word, _address| {
			// @TODO: Implement
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xffffffff,
		data: 0x00000073,
		name: "ECALL",
		operation: |cpu, _word, address| {
			let exception_type = match cpu.privilege_mode {
				PrivilegeMode::User => TrapType::EnvironmentCallFromUMode,
				PrivilegeMode::Supervisor => TrapType::EnvironmentCallFromSMode,
				PrivilegeMode::Machine => TrapType::EnvironmentCallFromMMode,
				PrivilegeMode::Reserved => panic!("Unknown Privilege mode")
			};
			return Err(Trap {
				trap_type: exception_type,
				value: address
			});
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xfe00007f,
		data: 0x02000053,
		name: "FADD.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.f[f.rs1] + cpu.f[f.rs2];
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0xd2200053,
		name: "FCVT.D.L",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.x[f.rs1] as f64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	// risc-box patch: the int↔double conversions upstream left out. Hit in
	// practice by busybox (e.g. ping converting monotonic nanoseconds).
	Instruction {
		mask: 0xfff0007f,
		data: 0xd2300053,
		name: "FCVT.D.LU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.x[f.rs1] as u64 as f64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0x42000053,
		name: "FCVT.D.S",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// Is this implementation correct?
			cpu.f[f.rd] = f32::from_bits(cpu.f[f.rs1].to_bits() as u32) as f64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0xd2000053,
		name: "FCVT.D.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.x[f.rs1] as i32 as f64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0xd2100053,
		name: "FCVT.D.WU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.x[f.rs1] as u32 as f64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0x40100053,
		name: "FCVT.S.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// Is this implementation correct?
			cpu.f[f.rd] = cpu.f[f.rs1] as f32 as f64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0xc2000053,
		name: "FCVT.W.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// risc-box patch: this converted through `as u32` (UNSIGNED), so any
			// negative double became 0 -- e.g. FCVT.W.D(-1.0) = 0. gcc lowers
			// (int32_t)double to exactly this instruction, so every negative
			// double->int cast in guest C code was wrong; V8's TurboFan constant
			// lowering (DoubleToInt32(-1.0)) turned -1 graph constants into 0 and
			// silently miscompiled JS. Signed saturating truncation, NaN -> MAX
			// per the RISC-V spec (Rust `as` gives NaN -> 0).
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = match a.is_nan() {
				true => i32::MAX as i64,
				false => a as i32 as i64
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	// risc-box patch: double→int conversions upstream left out (Rust `as`
	// saturates, matching RISC-V conversion semantics except NaN, which the
	// existing conversions above don't honor either).
	Instruction {
		mask: 0xfff0007f,
		data: 0xc2100053,
		name: "FCVT.WU.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// risc-box patch: NaN -> u32::MAX per spec (result sign-extended)
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = match a.is_nan() {
				true => u32::MAX as i32 as i64,
				false => a as u32 as i32 as i64
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0xc2200053,
		name: "FCVT.L.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// risc-box patch: NaN -> i64::MAX per spec
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = match a.is_nan() {
				true => i64::MAX,
				false => a as i64
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0xc2300053,
		name: "FCVT.LU.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// risc-box patch: NaN -> u64::MAX per spec
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = match a.is_nan() {
				true => u64::MAX as i64,
				false => a as u64 as i64
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00007f,
		data: 0x1a000053,
		name: "FDIV.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.f[f.rs1];
			let divisor = cpu.f[f.rs2];
			// Is this implementation correct?
			if divisor == 0.0 {
				cpu.f[f.rd] = std::f64::INFINITY;
				cpu.set_fcsr_dz();
			} else if divisor == -0.0 {
				cpu.f[f.rd] = std::f64::NEG_INFINITY;
				cpu.set_fcsr_dz();
			} else {
				cpu.f[f.rd] = dividend / divisor;
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x0000000f,
		name: "FENCE",
		operation: |_cpu, _word, _address| {
			// Do nothing?
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x0000100f,
		name: "FENCE.I",
		operation: |_cpu, _word, _address| {
			// Do nothing?
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0xa2002053,
		name: "FEQ.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.f[f.rs1] == cpu.f[f.rs2] {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00003007,
		name: "FLD",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.f[f.rd] = match cpu.mmu.load_doubleword(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => f64::from_bits(data),
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0xa2000053,
		name: "FLE.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.f[f.rs1] <= cpu.f[f.rs2] {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0xa2001053,
		name: "FLT.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.f[f.rs1] < cpu.f[f.rs2] {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00002007,
		name: "FLW",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.f[f.rd] = match cpu.mmu.load_word(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => f64::from_bits(data as i32 as i64 as u64),
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0x0600007f,
		data: 0x02000043,
		name: "FMADD.D",
		operation: |cpu, word, _address| {
			// @TODO: Update fcsr if needed?
			let f = parse_format_r2(word);
			cpu.f[f.rd] = cpu.f[f.rs1] * cpu.f[f.rs2] + cpu.f[f.rs3];
			Ok(())
		},
		disassemble: dump_format_r2
	},
	// risc-box patch: the rest of the common RV64D set upstream left out
	// (FMSUB/FNMADD complete the fused quartet; FMIN/FMAX; FSQRT).
	Instruction {
		mask: 0x0600007f,
		data: 0x02000047,
		name: "FMSUB.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r2(word);
			cpu.f[f.rd] = cpu.f[f.rs1] * cpu.f[f.rs2] - cpu.f[f.rs3];
			Ok(())
		},
		disassemble: dump_format_r2
	},
	Instruction {
		mask: 0x0600007f,
		data: 0x0200004f,
		name: "FNMADD.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r2(word);
			cpu.f[f.rd] = -(cpu.f[f.rs1] * cpu.f[f.rs2]) - cpu.f[f.rs3];
			Ok(())
		},
		disassemble: dump_format_r2
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x2a000053,
		name: "FMIN.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.f[f.rs1].min(cpu.f[f.rs2]);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x2a001053,
		name: "FMAX.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.f[f.rs1].max(cpu.f[f.rs2]);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0007f,
		data: 0x5a000053,
		name: "FSQRT.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.f[f.rs1].sqrt();
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00007f,
		data: 0x12000053,
		name: "FMUL.D",
		operation: |cpu, word, _address| {
			// @TODO: Update fcsr if needed?
			let f = parse_format_r(word);
			cpu.f[f.rd] = cpu.f[f.rs1] * cpu.f[f.rs2];
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0707f,
		data: 0xf2000053,
		name: "FMV.D.X",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = f64::from_bits(cpu.x[f.rs1] as u64);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0707f,
		data: 0xe2000053,
		name: "FMV.X.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.f[f.rs1].to_bits() as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0707f,
		data: 0xe0000053,
		name: "FMV.X.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.f[f.rs1].to_bits() as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfff0707f,
		data: 0xf0000053,
		name: "FMV.W.X",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.f[f.rd] = f64::from_bits(cpu.x[f.rs1] as u32 as u64);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0600007f,
		data: 0x0200004b,
		name: "FNMSUB.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r2(word);
			cpu.f[f.rd] = -(cpu.f[f.rs1] * cpu.f[f.rs2]) + cpu.f[f.rs3];
			Ok(())
		},
		disassemble: dump_format_r2
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00003027,
		name: "FSD",
		operation: |cpu, word, _address| {
			let f = parse_format_s(word);
			cpu.mmu.store_doubleword(cpu.x[f.rs1].wrapping_add(f.imm) as u64, cpu.f[f.rs2].to_bits())
		},
		disassemble: dump_format_s
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x22000053,
		name: "FSGNJ.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let rs1_bits = cpu.f[f.rs1].to_bits();
			let rs2_bits = cpu.f[f.rs2].to_bits();
			let sign_bit = rs2_bits & 0x8000000000000000;
			cpu.f[f.rd] = f64::from_bits(sign_bit | (rs1_bits & 0x7fffffffffffffff));
			Ok(())
		},
		disassemble: dump_format_r
	},
	// risc-box patch: FSGNJN.D (this is fneg.d — compilers emit it for every
	// double negation) was missing while its siblings above/below exist.
	Instruction {
		mask: 0xfe00707f,
		data: 0x22001053,
		name: "FSGNJN.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let rs1_bits = cpu.f[f.rs1].to_bits();
			let rs2_bits = cpu.f[f.rs2].to_bits();
			let sign_bit = !rs2_bits & 0x8000000000000000;
			cpu.f[f.rd] = f64::from_bits(sign_bit | (rs1_bits & 0x7fffffffffffffff));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x22002053,
		name: "FSGNJX.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let rs1_bits = cpu.f[f.rs1].to_bits();
			let rs2_bits = cpu.f[f.rs2].to_bits();
			let sign_bit = (rs1_bits ^ rs2_bits) & 0x8000000000000000;
			cpu.f[f.rd] = f64::from_bits(sign_bit | (rs1_bits & 0x7fffffffffffffff));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00007f,
		data: 0x0a000053,
		name: "FSUB.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// @TODO: Update fcsr if needed?
			cpu.f[f.rd] = cpu.f[f.rs1] - cpu.f[f.rs2];
			Ok(())
		},
		disassemble: dump_format_r
	},
		// ===== risc-box patch: the single-precision (RV64F) arithmetic set =====
		// Upstream implemented the full DOUBLE (.D) family but almost none of the
		// SINGLE (.S) one — only FCVT.D.S/FCVT.S.D + the FMV.{X.W,W.X} moves. Xorg
		// and glibc use single-precision floats constantly (window coordinates,
		// libm float paths), so the very first X screen setup SIGILL'd on FSGNJ.S
		// (word 0x20e705d3). Singles live in the low 32 bits of the f64 register
		// (the FMV.W.X convention): read f32::from_bits(bits as u32), write back
		// f64::from_bits(result.to_bits() as u64). fmt bits [26:25]=00 keep these
		// distinct from the .D encodings (fmt=01) that share the low opcode.
		Instruction {
			mask: 0xfe00007f, data: 0x00000053, name: "FADD.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((a + b).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00007f, data: 0x08000053, name: "FSUB.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((a - b).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00007f, data: 0x10000053, name: "FMUL.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((a * b).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00007f, data: 0x18000053, name: "FDIV.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				if b == 0.0 { cpu.set_fcsr_dz(); }
				cpu.f[f.rd] = f64::from_bits((a / b).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0x58000053, name: "FSQRT.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits(a.sqrt().to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x20000053, name: "FSGNJ.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let r1 = cpu.f[f.rs1].to_bits() as u32;
				let r2 = cpu.f[f.rs2].to_bits() as u32;
				cpu.f[f.rd] = f64::from_bits(((r2 & 0x80000000) | (r1 & 0x7fffffff)) as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x20001053, name: "FSGNJN.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let r1 = cpu.f[f.rs1].to_bits() as u32;
				let r2 = cpu.f[f.rs2].to_bits() as u32;
				cpu.f[f.rd] = f64::from_bits(((!r2 & 0x80000000) | (r1 & 0x7fffffff)) as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x20002053, name: "FSGNJX.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let r1 = cpu.f[f.rs1].to_bits() as u32;
				let r2 = cpu.f[f.rs2].to_bits() as u32;
				cpu.f[f.rd] = f64::from_bits(((( r1 ^ r2) & 0x80000000) | (r1 & 0x7fffffff)) as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x28000053, name: "FMIN.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits(a.min(b).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x28001053, name: "FMAX.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits(a.max(b).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0xa0002053, name: "FEQ.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.x[f.rd] = (a == b) as i64;
				Ok(())
			}, disassemble: dump_empty
		},
		Instruction {
			mask: 0xfe00707f, data: 0xa0001053, name: "FLT.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.x[f.rd] = (a < b) as i64;
				Ok(())
			}, disassemble: dump_empty
		},
		Instruction {
			mask: 0xfe00707f, data: 0xa0000053, name: "FLE.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				cpu.x[f.rd] = (a <= b) as i64;
				Ok(())
			}, disassemble: dump_empty
		},
		Instruction {
			mask: 0xfff0707f, data: 0xe0001053, name: "FCLASS.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let bits = cpu.f[f.rs1].to_bits() as u32;
				let sign = bits >> 31; let exp = (bits >> 23) & 0xff; let frac = bits & 0x7fffff;
				let c: u64 = if exp == 0xff && frac != 0 { if (frac >> 22) & 1 == 1 { 1 << 9 } else { 1 << 8 } }
					else if exp == 0xff { if sign == 1 { 1 << 0 } else { 1 << 7 } }
					else if exp == 0 && frac == 0 { if sign == 1 { 1 << 3 } else { 1 << 4 } }
					else if exp == 0 { if sign == 1 { 1 << 2 } else { 1 << 5 } }
					else { if sign == 1 { 1 << 1 } else { 1 << 6 } };
				cpu.x[f.rd] = c as i64;
				Ok(())
			}, disassemble: dump_format_r
		},
		// risc-box patch: FCLASS.D — the one RV64D instruction upstream left
		// out entirely. glibc's fpclassify/isnan paths compile to it; the
		// first process to classify a double SIGILLs without it (surfaced the
		// moment FP context started surviving context switches).
		Instruction {
			mask: 0xfff0707f, data: 0xe2001053, name: "FCLASS.D",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let bits = cpu.f[f.rs1].to_bits();
				let sign = bits >> 63;
				let exp = (bits >> 52) & 0x7ff;
				let frac = bits & 0xf_ffff_ffff_ffff;
				let c: u64 = if exp == 0x7ff && frac != 0 { if (frac >> 51) & 1 == 1 { 1 << 9 } else { 1 << 8 } }
					else if exp == 0x7ff { if sign == 1 { 1 << 0 } else { 1 << 7 } }
					else if exp == 0 && frac == 0 { if sign == 1 { 1 << 3 } else { 1 << 4 } }
					else if exp == 0 { if sign == 1 { 1 << 2 } else { 1 << 5 } }
					else { if sign == 1 { 1 << 1 } else { 1 << 6 } };
				cpu.x[f.rd] = c as i64;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0000053, name: "FCVT.W.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				cpu.x[f.rd] = a as i32 as i64;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0100053, name: "FCVT.WU.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				cpu.x[f.rd] = a as u32 as i32 as i64;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0200053, name: "FCVT.L.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				cpu.x[f.rd] = a as i64;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0300053, name: "FCVT.LU.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				cpu.x[f.rd] = a as u64 as i64;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0000053, name: "FCVT.S.W",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = f64::from_bits((cpu.x[f.rs1] as i32 as f32).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0100053, name: "FCVT.S.WU",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = f64::from_bits((cpu.x[f.rs1] as u32 as f32).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0200053, name: "FCVT.S.L",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = f64::from_bits((cpu.x[f.rs1] as i64 as f32).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0300053, name: "FCVT.S.LU",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = f64::from_bits((cpu.x[f.rs1] as u64 as f32).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0x0600007f, data: 0x00000043, name: "FMADD.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				let c = f32::from_bits(cpu.f[f.rs3].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((a * b + c).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r2
		},
		Instruction {
			mask: 0x0600007f, data: 0x00000047, name: "FMSUB.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				let c = f32::from_bits(cpu.f[f.rs3].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((a * b - c).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r2
		},
		Instruction {
			mask: 0x0600007f, data: 0x0000004b, name: "FNMSUB.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				let c = f32::from_bits(cpu.f[f.rs3].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((-(a * b) + c).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r2
		},
		Instruction {
			mask: 0x0600007f, data: 0x0000004f, name: "FNMADD.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = f32::from_bits(cpu.f[f.rs1].to_bits() as u32);
				let b = f32::from_bits(cpu.f[f.rs2].to_bits() as u32);
				let c = f32::from_bits(cpu.f[f.rs3].to_bits() as u32);
				cpu.f[f.rd] = f64::from_bits((-(a * b) - c).to_bits() as u64);
				Ok(())
			}, disassemble: dump_format_r2
		},
		// ===== end single-precision (RV64F) set =====
	Instruction {
		mask: 0x0000707f,
		data: 0x00002027,
		name: "FSW",
		operation: |cpu, word, _address| {
			let f = parse_format_s(word);
			cpu.mmu.store_word(cpu.x[f.rs1].wrapping_add(f.imm) as u64, cpu.f[f.rs2].to_bits() as u32)
		},
		disassemble: dump_format_s
	},
	Instruction {
		mask: 0x0000007f,
		data: 0x0000006f,
		name: "JAL",
		operation: |cpu, word, address| {
			let f = parse_format_j(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.pc as i64);
			cpu.pc = address.wrapping_add(f.imm);
			Ok(())
		},
		disassemble: dump_format_j
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00000067,
		name: "JALR",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			let tmp = cpu.sign_extend(cpu.pc as i64);
			cpu.pc = (cpu.x[f.rs1] as u64).wrapping_add(f.imm as u64);
			cpu.x[f.rd] = tmp;
			Ok(())
		},
		disassemble: |cpu, word, _address, evaluate| {
			let f = parse_format_i(word);
			let mut s = String::new();
			s += &format!("{}", get_register_name(f.rd));
			if evaluate {
				s += &format!(":{:x}", cpu.x[f.rd]);
			}
			s += &format!(",{:x}({}", f.imm, get_register_name(f.rs1));
			if evaluate {
				s += &format!(":{:x}", cpu.x[f.rs1]);
			}
			s += &format!(")");
			s
		}
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00000003,
		name: "LB",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i8 as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00004003,
		name: "LBU",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00003003,
		name: "LD",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load_doubleword(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00001003,
		name: "LH",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load_halfword(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i16 as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00005003,
		name: "LHU",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load_halfword(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0xf9f0707f,
		data: 0x1000302f,
		name: "LR.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// @TODO: Implement properly
			cpu.x[f.rd] = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => {
					cpu.is_reservation_set = true;
					cpu.reservation = cpu.x[f.rs1] as u64; // Is virtual address ok?
					data as i64
				},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf9f0707f,
		data: 0x1000202f,
		name: "LR.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// @TODO: Implement properly
			cpu.x[f.rd] = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => {
					cpu.is_reservation_set = true;
					cpu.reservation = cpu.x[f.rs1] as u64; // Is virtual address ok?
					data as i32 as i64
				},
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000007f,
		data: 0x00000037,
		name: "LUI",
		operation: |cpu, word, _address| {
			let f = parse_format_u(word);
			cpu.x[f.rd] = f.imm as i64;
			Ok(())
		},
		disassemble: dump_format_u
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00002003,
		name: "LW",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load_word(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i32 as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00006003,
		name: "LWU",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.mmu.load_word(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			Ok(())
		},
		disassemble: dump_format_i_mem
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02000033,
		name: "MUL",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_mul(cpu.x[f.rs2]));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02001033,
		name: "MULH",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.xlen {
				Xlen::Bit32 => {
					cpu.sign_extend((cpu.x[f.rs1] * cpu.x[f.rs2]) >> 32)
				},
				Xlen::Bit64 => {
					((cpu.x[f.rs1] as i128) * (cpu.x[f.rs2] as i128) >> 64) as i64
				}
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02003033,
		name: "MULHU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.xlen {
				Xlen::Bit32 => {
					cpu.sign_extend((((cpu.x[f.rs1] as u32 as u64) * (cpu.x[f.rs2] as u32 as u64)) >> 32) as i64)
				},
				Xlen::Bit64 => {
					((cpu.x[f.rs1] as u64 as u128).wrapping_mul(cpu.x[f.rs2] as u64 as u128) >> 64) as i64
				}
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02002033,
		name: "MULHSU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.xlen {
				Xlen::Bit32 => {
					cpu.sign_extend(((cpu.x[f.rs1] as i64).wrapping_mul(cpu.x[f.rs2] as u32 as i64) >> 32) as i64)
				},
				Xlen::Bit64 => {
					((cpu.x[f.rs1] as u128).wrapping_mul(cpu.x[f.rs2] as u64 as u128) >> 64) as i64
				}
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0200003b,
		name: "MULW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend((cpu.x[f.rs1] as i32).wrapping_mul(cpu.x[f.rs2] as i32) as i64);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xffffffff,
		data: 0x30200073,
		name: "MRET",
		operation: |cpu, _word, _address| {
			cpu.pc = match cpu.read_csr(CSR_MEPC_ADDRESS) {
				Ok(data) => data,
				Err(e) => return Err(e)
			};
			let status = cpu.read_csr_raw(CSR_MSTATUS_ADDRESS);
			let mpie = (status >> 7) & 1;
			let mpp = (status >> 11) & 0x3;
			let mprv = match get_privilege_mode(mpp) {
				PrivilegeMode::Machine => (status >> 17) & 1,
				_ => 0
			};
			// Override MIE[3] with MPIE[7], set MPIE[7] to 1, set MPP[12:11] to 0
			// and override MPRV[17]
			let new_status = (status & !0x21888) | (mprv << 17) | (mpie << 3) | (1 << 7);
			cpu.write_csr_raw(CSR_MSTATUS_ADDRESS, new_status);
			cpu.privilege_mode = match mpp {
				0 => PrivilegeMode::User,
				1 => PrivilegeMode::Supervisor,
				3 => PrivilegeMode::Machine,
				_ => panic!() // Shouldn't happen
			};
			cpu.mmu.update_privilege_mode(cpu.privilege_mode.clone());
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00006033,
		name: "OR",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] | cpu.x[f.rs2]);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00006013,
		name: "ORI",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] | f.imm);
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02006033,
		name: "REM",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.x[f.rs1];
			let divisor = cpu.x[f.rs2];
			if divisor == 0 {
				cpu.x[f.rd] = dividend;
			} else if dividend == cpu.most_negative() && divisor == -1 {
				cpu.x[f.rd] = 0;
			} else {
				cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_rem(cpu.x[f.rs2]));
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x02007033,
		name: "REMU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.unsigned_data(cpu.x[f.rs1]);
			let divisor = cpu.unsigned_data(cpu.x[f.rs2]);
			cpu.x[f.rd] = match divisor {
				0 => cpu.sign_extend(dividend as i64),
				_ => cpu.sign_extend(dividend.wrapping_rem(divisor) as i64)
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0200703b,
		name: "REMUW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.x[f.rs1] as u32;
			let divisor = cpu.x[f.rs2] as u32;
			cpu.x[f.rd] = match divisor {
				0 => dividend as i32 as i64,
				_ => dividend.wrapping_rem(divisor) as i32 as i64
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0200603b,
		name: "REMW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let dividend = cpu.x[f.rs1] as i32;
			let divisor = cpu.x[f.rs2] as i32;
			if divisor == 0 {
				cpu.x[f.rd] = dividend as i64;
			} else if dividend == std::i32::MIN && divisor == -1 {
				cpu.x[f.rd] = 0;
			} else {
				cpu.x[f.rd] = dividend.wrapping_rem(divisor) as i64;
			}
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00000023,
		name: "SB",
		operation: |cpu, word, _address| {
			let f = parse_format_s(word);
			cpu.mmu.store(cpu.x[f.rs1].wrapping_add(f.imm) as u64, cpu.x[f.rs2] as u8)
		},
		disassemble: dump_format_s
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x1800302f,
		name: "SC.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// @TODO: Implement properly
			cpu.x[f.rd] = match cpu.is_reservation_set && cpu.reservation == (cpu.x[f.rs1] as u64) {
				true => match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, cpu.x[f.rs2] as u64) {
					Ok(()) => {
						cpu.is_reservation_set = false;
						0
					},
					Err(e) => return Err(e)
				},
				false => {
					// risc-box patch: SC consumes the reservation win or lose
					cpu.is_reservation_set = false;
					1
				}
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xf800707f,
		data: 0x1800202f,
		name: "SC.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// @TODO: Implement properly
			cpu.x[f.rd] = match cpu.is_reservation_set && cpu.reservation == (cpu.x[f.rs1] as u64) {
				true => match cpu.mmu.store_word(cpu.x[f.rs1] as u64, cpu.x[f.rs2] as u32) {
					Ok(()) => {
						cpu.is_reservation_set = false;
						0
					},
					Err(e) => return Err(e)
				},
				false => {
					// risc-box patch: SC consumes the reservation win or lose
					cpu.is_reservation_set = false;
					1
				}
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00003023,
		name: "SD",
		operation: |cpu, word, _address| {
			let f = parse_format_s(word);
			cpu.mmu.store_doubleword(cpu.x[f.rs1].wrapping_add(f.imm) as u64, cpu.x[f.rs2] as u64)
		},
		disassemble: dump_format_s
	},
	Instruction {
		mask: 0xfe007fff,
		data: 0x12000073,
		name: "SFENCE.VMA",
		operation: |cpu, _word, _address| {
			// risc-box patch: was a no-op; the software TLB must honor it
			cpu.mmu.sfence_vma();
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00001023,
		name: "SH",
		operation: |cpu, word, _address| {
			let f = parse_format_s(word);
			cpu.mmu.store_halfword(cpu.x[f.rs1].wrapping_add(f.imm) as u64, cpu.x[f.rs2] as u16)
		},
		disassemble: dump_format_s
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00001033,
		name: "SLL",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_shl(cpu.x[f.rs2] as u32));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfc00707f,
		data: 0x00001013,
		name: "SLLI",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let mask = match cpu.xlen {
				Xlen::Bit32 => 0x1f,
				Xlen::Bit64 => 0x3f
			};
			let shamt = (word >> 20) & mask;
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] << shamt);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0000101b,
		name: "SLLIW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let shamt = f.rs2 as u32;
			cpu.x[f.rd] = (cpu.x[f.rs1] << shamt) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0000103b,
		name: "SLLW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = (cpu.x[f.rs1] as u32).wrapping_shl(cpu.x[f.rs2] as u32) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00002033,
		name: "SLT",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.x[f.rs1] < cpu.x[f.rs2] {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00002013,
		name: "SLTI",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.x[f.rs1] < f.imm {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00003013,
		name: "SLTIU",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = match cpu.unsigned_data(cpu.x[f.rs1]) < cpu.unsigned_data(f.imm) {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_format_i
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00003033,
		name: "SLTU",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = match cpu.unsigned_data(cpu.x[f.rs1]) < cpu.unsigned_data(cpu.x[f.rs2]) {
				true => 1,
				false => 0
			};
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x40005033,
		name: "SRA",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_shr(cpu.x[f.rs2] as u32));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfc00707f,
		data: 0x40005013,
		name: "SRAI",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let mask = match cpu.xlen {
				Xlen::Bit32 => 0x1f,
				Xlen::Bit64 => 0x3f
			};
			let shamt = (word >> 20) & mask;
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] >> shamt);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfc00707f,
		data: 0x4000501b,
		name: "SRAIW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let shamt = ((word >> 20) & 0x1f) as u32;
			cpu.x[f.rd] = ((cpu.x[f.rs1] as i32) >> shamt) as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x4000503b,
		name: "SRAW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = (cpu.x[f.rs1] as i32).wrapping_shr(cpu.x[f.rs2] as u32) as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xffffffff,
		data: 0x10200073,
		name: "SRET",
		operation: |cpu, _word, _address| {
			// @TODO: Throw error if higher privilege return instruction is executed
			cpu.pc = match cpu.read_csr(CSR_SEPC_ADDRESS) {
				Ok(data) => data,
				Err(e) => return Err(e)
			};
			let status = cpu.read_csr_raw(CSR_SSTATUS_ADDRESS);
			let spie = (status >> 5) & 1;
			let spp = (status >> 8) & 1;
			let mprv = match get_privilege_mode(spp) {
				PrivilegeMode::Machine => (status >> 17) & 1,
				_ => 0
			};
			// Override SIE[1] with SPIE[5], set SPIE[5] to 1, set SPP[8] to 0,
			// and override MPRV[17]
			let new_status = (status & !0x20122) | (mprv << 17) | (spie << 1) | (1 << 5);
			cpu.write_csr_raw(CSR_SSTATUS_ADDRESS, new_status);
			cpu.privilege_mode = match spp {
				0 => PrivilegeMode::User,
				1 => PrivilegeMode::Supervisor,
				_ => panic!() // Shouldn't happen
			};
			cpu.mmu.update_privilege_mode(cpu.privilege_mode.clone());
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00005033,
		name: "SRL",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.unsigned_data(cpu.x[f.rs1]).wrapping_shr(cpu.x[f.rs2] as u32) as i64);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfc00707f,
		data: 0x00005013,
		name: "SRLI",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let mask = match cpu.xlen {
				Xlen::Bit32 => 0x1f,
				Xlen::Bit64 => 0x3f
			};
			let shamt = (word >> 20) & mask;
			cpu.x[f.rd] = cpu.sign_extend((cpu.unsigned_data(cpu.x[f.rs1]) >> shamt) as i64);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfc00707f,
		data: 0x0000501b,
		name: "SRLIW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let mask = match cpu.xlen {
				Xlen::Bit32 => 0x1f,
				Xlen::Bit64 => 0x3f
			};
			let shamt = (word >> 20) & mask;
			cpu.x[f.rd] = ((cpu.x[f.rs1] as u32) >> shamt) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x0000503b,
		name: "SRLW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = (cpu.x[f.rs1] as u32).wrapping_shr(cpu.x[f.rs2] as u32) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x40000033,
		name: "SUB",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1].wrapping_sub(cpu.x[f.rs2]));
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x4000003b,
		name: "SUBW",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.x[f.rs1].wrapping_sub(cpu.x[f.rs2]) as i32 as i64;
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00002023,
		name: "SW",
		operation: |cpu, word, _address| {
			let f = parse_format_s(word);
			cpu.mmu.store_word(cpu.x[f.rs1].wrapping_add(f.imm) as u64, cpu.x[f.rs2] as u32)
		},
		disassemble: dump_format_s
	},
	Instruction {
		mask: 0xffffffff,
		data: 0x00200073,
		name: "URET",
		operation: |_cpu, _word, _address| {
			// @TODO: Implement
			panic!("URET instruction is not implemented yet.");
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xffffffff,
		data: 0x10500073,
		name: "WFI",
		operation: |cpu, _word, _address| {
			cpu.wfi = true;
			Ok(())
		},
		disassemble: dump_empty
	},
	Instruction {
		mask: 0xfe00707f,
		data: 0x00004033,
		name: "XOR",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] ^ cpu.x[f.rs2]);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0x0000707f,
		data: 0x00004013,
		name: "XORI",
		operation: |cpu, word, _address| {
			let f = parse_format_i(word);
			cpu.x[f.rd] = cpu.sign_extend(cpu.x[f.rs1] ^ f.imm);
			Ok(())
		},
		disassemble: dump_format_i
	},
];

/// The number of results [`DecodeCache`](struct.DecodeCache.html) holds.
/// You need to carefully choose the number. Too small number causes
/// bad cache hit ratio. Too large number causes memory consumption
/// and host hardware CPU cache memory miss.
const DECODE_CACHE_ENTRY_NUM: usize = 0x4000; // risc-box patch: was 0x1000

// risc-box patch: marks a 4-byte (non-compressed) instruction in the
// INSTRUCTIONS-index field of a predecoded BlockOp.
const ICACHE_LEN4: u16 = 0x8000;

// risc-box patch: tag layout for the direct-mapped cache below — the decoded
// word plus a valid bit above bit 31, so no 32-bit word value (0, all-ones)
// can false-hit against an empty slot.
const DECODE_TAG_VALID: u64 = 1 << 32;

/// `DecodeCache` provides a cache system for instruction decoding.
/// It holds the recent [`DECODE_CACHE_ENTRY_NUM`](constant.DECODE_CACHE_ENTRY_NUM.html)
/// instruction decode results. If it has a cache (called "hit") for passed
/// word data, it returns decoding result very quickly. Decoding is one of the
/// slowest parts in CPU. This cache system improves the CPU processing speed
/// by skipping decoding. Especially it should work well for loop. It is said
/// that some loops in a program consume the majority of time then this cache
/// system is expected to reduce the decoding time very well.
///
/// risc-box patch: the original implementation was an FnvHashMap plus a
/// doubly-linked LRU list — a hash, a probe, and a three-node list splice
/// on every HIT, in the interpreter's hottest path. Decoding is a pure
/// function of the word, so eviction policy is only a hit-rate concern;
/// this direct-mapped table trades a little hit rate for a lookup that is
/// a shift, a mask, and one compare.
struct DecodeCache {
	/// `word | DECODE_TAG_VALID` per slot; 0 = empty slot
	tags: Vec<u64>,

	/// The decode result per slot. An index of [`INSTRUCTIONS`](constant.INSTRUCTIONS.html).
	vals: Vec<usize>,

	/// Cache hit count for debugging purpose
	hit_count: u64,

	/// Cache miss count for debugging purpose
	miss_count: u64
}

impl DecodeCache {
	/// Creates a new `DecodeCache`.
	fn new() -> Self {
		DecodeCache {
			tags: vec![0; DECODE_CACHE_ENTRY_NUM],
			vals: vec![0; DECODE_CACHE_ENTRY_NUM],
			hit_count: 0,
			miss_count: 0
		}
	}

	/// The slot a word maps to. The low two bits of a full-width RISC-V
	/// instruction are always 0b11, so they are shifted out; higher funct
	/// bits are folded in for spread.
	fn slot(word: u32) -> usize {
		(((word >> 2) ^ (word >> 17)) as usize) & (DECODE_CACHE_ENTRY_NUM - 1)
	}

	/// Gets the cached decoding result as an index of
	/// [`INSTRUCTIONS`](constant.INSTRUCTIONS.html), or `None` on miss.
	///
	/// # Arguments
	/// * `word` word instruction data
	fn get(&mut self, word: u32) -> Option<usize> {
		let slot = DecodeCache::slot(word);
		match self.tags[slot] == word as u64 | DECODE_TAG_VALID {
			true => {
				self.hit_count += 1;
				Some(self.vals[slot])
			},
			false => {
				self.miss_count += 1;
				None
			}
		}
	}

	/// Inserts a new decode result, evicting whatever occupied the slot.
	///
	/// # Arguments
	/// * `word`
	/// * `instruction_index`
	fn insert(&mut self, word: u32, instruction_index: usize) {
		let slot = DecodeCache::slot(word);
		self.tags[slot] = word as u64 | DECODE_TAG_VALID;
		self.vals[slot] = instruction_index;
	}
}

#[cfg(test)]
mod test_cpu {
	use terminal::DummyTerminal;
	use mmu::DRAM_BASE;
	use super::*;

	fn create_cpu() -> Cpu {
		Cpu::new(Box::new(DummyTerminal::new()))
	}

	#[test]
	fn initialize() {
		let _cpu = create_cpu();
	}

	#[test]
	fn update_pc() {
		let mut cpu = create_cpu();
		assert_eq!(0, cpu.read_pc());
		cpu.update_pc(1);
		assert_eq!(1, cpu.read_pc());
		cpu.update_pc(0xffffffffffffffff);
		assert_eq!(0xffffffffffffffff, cpu.read_pc());
	}

	#[test]
	fn update_xlen() {
		let mut cpu = create_cpu();
		assert!(matches!(cpu.xlen, Xlen::Bit64));
		cpu.update_xlen(Xlen::Bit32);
		assert!(matches!(cpu.xlen, Xlen::Bit32));
		cpu.update_xlen(Xlen::Bit64);
		assert!(matches!(cpu.xlen, Xlen::Bit64));
		// Note: cpu.update_xlen() updates cpu.mmu.xlen, too.
		// The test for mmu.xlen should be in Mmu?
	}

	#[test]
	fn read_register() {
		let mut cpu = create_cpu();
		// Initial register values are 0 other than 0xb th register.
		// Initial value of 0xb th register is temporal for Linux boot and
		// I'm not sure if the value is correct. Then skipping so far.
		for i in 0..31 {
			if i != 0xb {
				assert_eq!(0, cpu.read_register(i));
			}
		}

		for i in 0..31 {
			cpu.x[i] = i as i64 + 1;
		}

		for i in 0..31 {
			match i {
				// 0th register is hardwired zero
				0 => assert_eq!(0, cpu.read_register(i)),
				_ => assert_eq!(i as i64 + 1, cpu.read_register(i))
			}
		}

		for i in 0..31 {
			cpu.x[i] = (0xffffffffffffffff - i) as i64;
		}

		for i in 0..31 {
			match i {
				// 0th register is hardwired zero
				0 => assert_eq!(0, cpu.read_register(i)),
				_ => assert_eq!(-(i as i64 + 1), cpu.read_register(i))
			}
		}

		// @TODO: Should I test the case where the argument equals to or is
		// greater than 32?
	}

	#[test]
	fn tick() {
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(4);
		cpu.update_pc(DRAM_BASE);

		// Write non-compressed "addi x1, x1, 1" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0x00108093) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		// Write compressed "addi x8, x0, 8" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE + 4, 0x20) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};

		cpu.tick();

		assert_eq!(DRAM_BASE + 4, cpu.read_pc());
		assert_eq!(1, cpu.read_register(1));

		cpu.tick();

		assert_eq!(DRAM_BASE + 6, cpu.read_pc());
		assert_eq!(8, cpu.read_register(8));
	}

	#[test]
	fn tick_operate() {
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(4);
		cpu.update_pc(DRAM_BASE);
		// write non-compressed "addi a0, a0, 12" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0xc50513) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		assert_eq!(DRAM_BASE, cpu.read_pc());
		assert_eq!(0, cpu.read_register(10));
		match cpu.tick_operate() {
			Ok(()) => {},
			Err(_e) => panic!("tick_operate() unexpectedly did panic")
		};
		// .tick_operate() increments the program counter by 4 for
		// non-compressed instruction.
		assert_eq!(DRAM_BASE + 4, cpu.read_pc());
		// "addi a0, a0, a12" instruction writes 12 to a0 register.
		assert_eq!(12, cpu.read_register(10));
		// @TODO: Test compressed instruction operation
	}

	#[test]
	fn fetch() {
		// .fetch() reads four bytes from the memory
		// at the address the program counter points to.
		// .fetch() doesn't increment the program counter.
		// .tick_operate() does.
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(4);
		cpu.update_pc(DRAM_BASE);
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0xaaaaaaaa) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		match cpu.fetch() {
			Ok(data) => assert_eq!(0xaaaaaaaa, data),
			Err(_e) => panic!("Failed to fetch")
		};
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0x55555555) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		match cpu.fetch() {
			Ok(data) => assert_eq!(0x55555555, data),
			Err(_e) => panic!("Failed to fetch")
		};
		// @TODO: Write test cases where Trap happens
	}

	#[test]
	fn decode() {
		let mut cpu = create_cpu();
		// 0x13 is addi instruction
		match cpu.decode(0x13) {
			Ok(inst) => assert_eq!(inst.name, "ADDI"),
			Err(_e) => panic!("Failed to decode")
		};
		// .decode() returns error for invalid word data.
		match cpu.decode(0x0) {
			Ok(_inst) => panic!("Unexpectedly succeeded in decoding"),
			Err(()) => assert!(true)
		};
		// @TODO: Should I test all instructions?
	}

	#[test]
	fn uncompress() {
		let mut cpu = create_cpu();
		// .uncompress() doesn't directly return an instruction but
		// it returns uncompressed word. Then you need to call .decode().
		match cpu.decode(cpu.uncompress(0x20)) {
			Ok(inst) => assert_eq!(inst.name, "ADDI"),
			Err(_e) => panic!("Failed to decode")
		};
		// @TODO: Should I test all compressed instructions?
	}

	#[test]
	fn wfi() {
		let wfi_instruction = 0x10500073;
		let mut cpu = create_cpu();
		// Just in case
		match cpu.decode(wfi_instruction) {
			Ok(inst) => assert_eq!(inst.name, "WFI"),
			Err(_e) => panic!("Failed to decode")
		};
		cpu.get_mut_mmu().init_memory(4);
		cpu.update_pc(DRAM_BASE);
		// write WFI instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, wfi_instruction) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		cpu.tick();
		assert_eq!(DRAM_BASE + 4, cpu.read_pc());
		for _i in 0..10 {
			// Until interrupt happens, .tick() does nothing
			// @TODO: Check accurately that the state is unchanged
			cpu.tick();
			assert_eq!(DRAM_BASE + 4, cpu.read_pc());
		}
		// Machine timer interrupt
		cpu.write_csr_raw(CSR_MIE_ADDRESS, MIP_MTIP);
		cpu.write_csr_raw(CSR_MIP_ADDRESS, MIP_MTIP);
		cpu.write_csr_raw(CSR_MSTATUS_ADDRESS, 0x8);
		cpu.write_csr_raw(CSR_MTVEC_ADDRESS, 0x0);
		cpu.tick();
		// Interrupt happened and moved to handler
		assert_eq!(0, cpu.read_pc());
	}

	#[test]
	fn interrupt() {
		let handler_vector = 0x10000000;
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(4);
		// Write non-compressed "addi x0, x0, 1" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0x00100013) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		cpu.update_pc(DRAM_BASE);

		// Machine timer interrupt but mie in mstatus is not enabled yet
		cpu.write_csr_raw(CSR_MIE_ADDRESS, MIP_MTIP);
		cpu.write_csr_raw(CSR_MIP_ADDRESS, MIP_MTIP);
		cpu.write_csr_raw(CSR_MTVEC_ADDRESS, handler_vector);

		cpu.tick();

		// Interrupt isn't caught because mie is disabled
		assert_eq!(DRAM_BASE + 4, cpu.read_pc());

		cpu.update_pc(DRAM_BASE);
		// Enable mie in mstatus
		cpu.write_csr_raw(CSR_MSTATUS_ADDRESS, 0x8);

		cpu.tick();

		// Interrupt happened and moved to handler
		assert_eq!(handler_vector, cpu.read_pc());

		// CSR Cause register holds the reason what caused the interrupt
		assert_eq!(0x8000000000000007, cpu.read_csr_raw(CSR_MCAUSE_ADDRESS));

		// @TODO: Test post CSR status register
		// @TODO: Test xIE bit in CSR status register
		// @TODO: Test privilege levels
		// @TODO: Test delegation
		// @TODO: Test vector type handlers
	}

	#[test]
	fn exception() {
		let handler_vector = 0x10000000;
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(4);
		// Write ECALL instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0x00000073) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		cpu.write_csr_raw(CSR_MTVEC_ADDRESS, handler_vector);
		cpu.update_pc(DRAM_BASE);

		cpu.tick();

		// Interrupt happened and moved to handler
		assert_eq!(handler_vector, cpu.read_pc());

		// CSR Cause register holds the reason what caused the trap
		assert_eq!(0xb, cpu.read_csr_raw(CSR_MCAUSE_ADDRESS));

		// @TODO: Test post CSR status register
		// @TODO: Test privilege levels
		// @TODO: Test delegation
		// @TODO: Test vector type handlers
	}

	#[test]
	fn hardocded_zero() {
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(8);
		cpu.update_pc(DRAM_BASE);

		// Write non-compressed "addi x0, x0, 1" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0x00100013) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};
		// Write non-compressed "addi x1, x1, 1" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE + 4, 0x00108093) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};

		// Test x0
		assert_eq!(0, cpu.read_register(0));
		cpu.tick(); // Execute  "addi x0, x0, 1"
		// x0 is still zero because it's hardcoded zero
		assert_eq!(0, cpu.read_register(0));

		// Test x1
		assert_eq!(0, cpu.read_register(1));
		cpu.tick(); // Execute  "addi x1, x1, 1"
		// x1 is not hardcoded zero
		assert_eq!(1, cpu.read_register(1));
	}

	#[test]
	fn disassemble_next_instruction() {
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(4);
		cpu.update_pc(DRAM_BASE);

		// Write non-compressed "addi x0, x0, 1" instruction
		match cpu.get_mut_mmu().store_word(DRAM_BASE, 0x00100013) {
			Ok(()) => {},
			Err(_e) => panic!("Failed to store")
		};

		assert_eq!("PC:0000000080000000 00100013 ADDI zero:0,zero:0,1",
			cpu.disassemble_next_instruction());

		// No effect to PC
		assert_eq!(DRAM_BASE, cpu.read_pc());
	}
}

#[cfg(test)]
mod test_dump_uncompress {
	use super::*;
	use terminal::DummyTerminal;

	// Not an assertion: dumps every 16-bit halfword's uncompress() expansion so
	// an external reference decoder can diff it (the C.FLDSP rd==0 bug class).
	// Run with: cargo test dump_uncompress -- --ignored
	#[test]
	#[ignore]
	fn dump_uncompress() {
		let cpu = Cpu::new(Box::new(DummyTerminal::new()));
		let mut out = String::with_capacity(0x10000 * 14);
		for hw in 0..0x10000u32 {
			if hw & 0x3 == 0x3 { continue; } // not a compressed encoding
			let w = cpu.uncompress(hw);
			out.push_str(&format!("{:04x}\t{:08x}\n", hw, w));
		}
		std::fs::write("/tmp/uncompress-dump.tsv", out).unwrap();
	}
}

#[cfg(test)]

mod test_decode_cache {
	use super::*;

	#[test]
	fn initialize() {
		let _cache = DecodeCache::new();
	}

	#[test]
	fn insert() {
		let mut cache = DecodeCache::new();
		cache.insert(0, 0);
	}

	#[test]
	fn get() {
		let mut cache = DecodeCache::new();
		cache.insert(1, 2);

		// Cache hit test
		match cache.get(1) {
			Some(index) => assert_eq!(2, index),
			None => panic!("Unexpected cache miss")
		};

		// Cache miss test
		match cache.get(2) {
			Some(_index) => panic!("Unexpected cache hit"),
			None => {}
		};
	}

	// risc-box patch: the cache is direct-mapped now (LRU is gone). Colliding
	// words evict each other; non-colliding words coexist regardless of age.
	#[test]
	fn direct_mapped() {
		let mut cache = DecodeCache::new();
		cache.insert(0, 1);

		match cache.get(0) {
			Some(index) => assert_eq!(1, index),
			None => panic!("Unexpected cache miss")
		};

		// Non-colliding words (slots 1, 2, 3) coexist with word 0 (slot 0)
		cache.insert(4, 10);
		cache.insert(8, 11);
		cache.insert(12, 12);
		match cache.get(0) {
			Some(index) => assert_eq!(1, index),
			None => panic!("Unexpected cache miss")
		};

		// 0x20004 hashes to slot 0 too and must evict word 0
		assert_eq!(DecodeCache::slot(0), DecodeCache::slot(0x20004));
		cache.insert(0x20004, 7);
		match cache.get(0) {
			Some(_index) => panic!("Unexpected cache hit"),
			None => {}
		};
		match cache.get(0x20004) {
			Some(index) => assert_eq!(7, index),
			None => panic!("Unexpected cache miss")
		};

		// The non-colliding neighbors are untouched by the eviction
		match cache.get(8) {
			Some(index) => assert_eq!(11, index),
			None => panic!("Unexpected cache miss")
		};
	}
}

// risc-box patch (jit feature): equivalence between the translator
// (src/jit.rs) and the REAL exec_block, on randomized op sequences over
// the supported integer subset. This lives here because it compares
// private machine state.
#[cfg(all(test, feature = "jit"))]
mod test_jit_equivalence {
	extern crate wasmtime;
	use super::*;
	use jit;
	use mmu::DRAM_BASE;
	use terminal::DummyTerminal;

	const XB: u32 = 0; // x[32] at 0
	const PCA: u32 = 256;
	const GENA: u32 = 264;
	const FB: u32 = 512; // f[32] as raw 8-byte cells
	const DB: u32 = 4096; // linear offset of guest DRAM window
	const WIN: u64 = 64 * 1024; // mirrored DRAM window size

	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> u64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			self.0
		}
	}

	fn rand_ops(r: &mut Rng, len: usize) -> Vec<BlockOp> {
		let mut ops = Vec::new();
		for _ in 0..len {
			let rd = match (r.next() % 32) as u8 {
				v @ 10..=13 => v + 10,
				v => v,
			};
			let ra = (r.next() % 32) as u8;
			let rb = (r.next() % 32) as u8;
			let p = (10 + r.next() % 4) as u8; // stable pointer regs
			let imm12 = ((r.next() % 4096) as i32) - 2048;
			let mem_imm = ((r.next() % 2048) as i32) & !7; // 0..2040, aligned
			let shamt6 = (r.next() % 64) as u32;
			let shamt5 = (r.next() % 32) as u32;
			let bimm = (((r.next() % 512) as i32) - 256) & !1; // branch offset, even
			let (kind, rrd, rrs1, rrs2, imm, word) = match r.next() % 55 {
				0 => (HOT_ADDI, rd, ra, 0, imm12, 0),
				1 => (HOT_ADD, rd, ra, rb, 0, 0),
				2 => (HOT_SUB, rd, ra, rb, 0, 0),
				3 => (HOT_AND, rd, ra, rb, 0, 0),
				4 => (HOT_OR, rd, ra, rb, 0, 0),
				5 => (HOT_XOR, rd, ra, rb, 0, 0),
				6 => (HOT_ANDI, rd, ra, 0, imm12, 0),
				7 => (HOT_ORI, rd, ra, 0, imm12, 0),
				8 => (HOT_XORI, rd, ra, 0, imm12, 0),
				9 => (HOT_MUL, rd, ra, rb, 0, 0),
				10 => (HOT_SLL, rd, ra, rb, 0, 0),
				11 => (HOT_SRL, rd, ra, rb, 0, 0),
				12 => (HOT_SRA, rd, ra, rb, 0, 0),
				13 => (HOT_SLLI, rd, ra, 0, 0, shamt6 << 20),
				14 => (HOT_SRLI, rd, ra, 0, 0, shamt6 << 20),
				15 => (HOT_SRAI, rd, ra, 0, 0, shamt6 << 20),
				16 => (HOT_LUI, rd, 0, 0, ((r.next() as i32) & !0xfff), 0),
				17 => (HOT_AUIPC, rd, 0, 0, ((r.next() as i32) & !0xfff), 0),
				18 => (HOT_ADDIW, rd, ra, 0, imm12, 0),
				19 => (HOT_ADDW, rd, ra, rb, 0, 0),
				20 => (HOT_SUBW, rd, ra, rb, 0, 0),
				21 => (HOT_SRAIW, rd, ra, 0, 0, shamt5 << 20),
				22 => (HOT_LD, rd, p, 0, mem_imm, 0),
				23 => (HOT_SD, 0, p, rb, mem_imm, 0),
				24 => (HOT_LW, rd, p, 0, mem_imm, 0),
				25 => (HOT_LWU, rd, p, 0, mem_imm, 0),
				26 => (HOT_LH, rd, p, 0, mem_imm, 0),
				27 => (HOT_LHU, rd, p, 0, mem_imm, 0),
				28 => (HOT_LB, rd, p, 0, mem_imm, 0),
				29 => (HOT_LBU, rd, p, 0, mem_imm, 0),
				30 => (HOT_SW, 0, p, rb, mem_imm, 0),
				31 => (HOT_SH, 0, p, rb, mem_imm, 0),
				32 => (HOT_SB, 0, p, rb, mem_imm, 0),
				33 => (HOT_SLT, rd, ra, rb, 0, 0),
				34 => (HOT_SLTU, rd, ra, rb, 0, 0),
				35 => (HOT_SLTI, rd, ra, 0, imm12, 0),
				36 => (HOT_SLTIU, rd, ra, 0, imm12, 0),
				37 => match r.next() % 6 {
					0 => (HOT_BEQ, 0, ra, rb, bimm, 0),
					1 => (HOT_BNE, 0, ra, rb, bimm, 0),
					2 => (HOT_BLT, 0, ra, rb, bimm, 0),
					3 => (HOT_BGE, 0, ra, rb, bimm, 0),
					4 => (HOT_BLTU, 0, ra, rb, bimm, 0),
					_ => (HOT_BGEU, 0, ra, rb, bimm, 0),
				},
				38 => (HOT_JAL, rd, 0, 0, bimm, 0),
				39 => (HOT_JALR, rd, ra, 0, imm12, 0),
				40 => (HOT_SLLIW, rd, ra, shamt5 as u8, 0, 0),
				41 => (HOT_SLLW, rd, ra, rb, 0, 0),
				42 => (HOT_SRLW, rd, ra, rb, 0, 0),
				43 => (HOT_SRAW, rd, ra, rb, 0, 0),
				44 => (HOT_FLD, rd, p, 0, mem_imm, 0),
				45 => (HOT_FSD, 0, p, rb, mem_imm, 0),
				46 => (HOT_FADD_D, rd, ra, rb, 0, 0),
				47 => (HOT_FSUB_D, rd, ra, rb, 0, 0),
				48 => (HOT_FMUL_D, rd, ra, rb, 0, 0),
				49 => (HOT_FSGNJ_D, rd, ra, rb, 0, 0),
				50 => (HOT_FMV_X_D, rd, ra, 0, 0, 0),
				51 => (HOT_FMV_D_X, rd, ra, 0, 0, 0),
				52 => (HOT_FLW, rd, p, 0, mem_imm, 0),
				53 => (HOT_FSW, 0, p, rb, mem_imm, 0),
				_ => (HOT_FCVT_D_W, rd, ra, 0, 0, 0),
			};
			let _ = shamt5;
			ops.push(BlockOp {
				imm: imm,
				word: word,
				data: 0,
				kind: kind,
				rd: rrd,
				rs1: rrs1,
				rs2: rrs2,
				len: 4,
				_pad: 0,
			});
		}
		ops
	}

	fn fresh_cpu(r: &mut Rng) -> Cpu {
		let mut cpu = Cpu::new(Box::new(DummyTerminal::new()));
		cpu.get_mut_mmu().init_memory(WIN);
		for i in 1..32 {
			cpu.x[i] = r.next() as i64;
		}
		for p in 10..14 {
			cpu.x[p] = (DRAM_BASE + 8192 + (r.next() % 16384 & !7)) as i64;
		}
		cpu.x[0] = 0;
		for i in 0..32 {
			// finite doubles only: NaN payload propagation differs between
			// the native interpreter (host FPU) and engine-canonicalized
			// wasm in some configurations; production runs BOTH sides under
			// the same engine, so finite inputs are the honest test domain
			let m = (r.next() % 2000000) as f64 / 1000.0 - 1000.0;
			cpu.f[i] = m;
		}
		for a in (0..WIN).step_by(8) {
			let v = r.next();
			let _ = cpu.get_mut_mmu().store_doubleword(DRAM_BASE + a, v);
		}
		cpu
	}

	fn run_wasm(bytes: &[u8], cpu_pre: &Cpu, start: u64) -> (u64, [i64; 32], u64, Vec<u8>, [u64; 32]) {
		let engine = wasmtime::Engine::default();
		let module = wasmtime::Module::new(&engine, bytes).expect("valid module");
		let mut store = wasmtime::Store::new(&engine, ());
		let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
		{
			let d = mem.data_mut(&mut store);
			for i in 0..32 {
				d[XB as usize + i * 8..XB as usize + i * 8 + 8]
					.copy_from_slice(&cpu_pre.x[i].to_le_bytes());
			}
			d[PCA as usize..PCA as usize + 8].copy_from_slice(&start.to_le_bytes());
			for i in 0..32 {
				d[FB as usize + i * 8..FB as usize + i * 8 + 8]
					.copy_from_slice(&cpu_pre.f[i].to_bits().to_le_bytes());
			}
			d[GENA as usize..GENA as usize + 4]
				.copy_from_slice(&cpu_pre.mmu.code_gen().to_le_bytes());
			let mut win = vec![0u8; WIN as usize];
			cpu_pre.mmu.read_physical_range(DRAM_BASE, &mut win);
			d[DB as usize..DB as usize + WIN as usize].copy_from_slice(&win);
		}
		let instance = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
		let run = instance.get_typed_func::<(), i64>(&mut store, "run").unwrap();
		let retired = run.call(&mut store, ()).unwrap() as u64;
		let d = mem.data(&store);
		let mut x = [0i64; 32];
		for i in 0..32 {
			let mut b = [0u8; 8];
			b.copy_from_slice(&d[XB as usize + i * 8..XB as usize + i * 8 + 8]);
			x[i] = i64::from_le_bytes(b);
		}
		let mut b = [0u8; 8];
		b.copy_from_slice(&d[PCA as usize..PCA as usize + 8]);
		let pc = u64::from_le_bytes(b);
		let dram = d[DB as usize..DB as usize + WIN as usize].to_vec();
		let mut f = [0u64; 32];
		for i in 0..32 {
			let mut b = [0u8; 8];
			b.copy_from_slice(&d[FB as usize + i * 8..FB as usize + i * 8 + 8]);
			f[i] = u64::from_le_bytes(b);
		}
		(retired, x, pc, dram, f)
	}

	#[test]
	fn translator_matches_exec_block() {
		let mut checked = 0;
		for seed in 1..400u64 {
			let mut r = Rng(seed * 2654435761 | 1);
			let len = 2 + (r.next() % 12) as usize;
			let ops = rand_ops(&mut r, len);
			let start = DRAM_BASE; // block's pc; DRAM phys tag irrelevant here
			let mut cpu = fresh_cpu(&mut Rng(seed * 40503 | 1));
			let lay = jit::Layout {
				x_base: XB,
				f_base: FB,
				tlb: None,
				pc_addr: PCA,
				gen_addr: GENA,
				baked_gen: cpu.mmu.code_gen(),
				dram_base: DB,
				guest_dram_base: DRAM_BASE,
				dram_len: WIN,
			};
			let bytes = match jit::emit_block(&ops, start, &lay) {
				Some(b) => b,
				None => continue,
			};
			// wasm first (from the pristine state), then the real engine
			let (rw, xw, pcw, dramw, fw) = run_wasm(&bytes, &cpu, start);
			cpu.update_pc(start);
			cpu.install_block_for_test(0, start, 0, &ops);
			let ri = cpu.exec_block(0);
			assert_eq!(ri, rw, "retired mismatch seed {}", seed);
			assert_eq!(cpu.x, xw, "registers mismatch seed {}", seed);
			assert_eq!(cpu.pc, pcw, "pc mismatch seed {}", seed);
			let mut dram_i = vec![0u8; WIN as usize];
			cpu.mmu.read_physical_range(DRAM_BASE, &mut dram_i);
			assert_eq!(dram_i, dramw, "dram mismatch seed {}", seed);
			for i in 0..32 {
				assert_eq!(cpu.f[i].to_bits(), fw[i], "f{} mismatch seed {}", i, seed);
			}
			checked += 1;
		}
		assert!(checked > 300, "too few cases ran: {}", checked);
	}

	fn op(kind: u8, rd: u8, rs1: u8, rs2: u8, imm: i32) -> BlockOp {
		BlockOp { imm: imm, word: 0, data: 0, kind: kind, rd: rd, rs1: rs1, rs2: rs2, len: 4, _pad: 0 }
	}

	/// Reference: interpreter-dispatch the region until pc leaves it or the
	/// fuel bound is met at a block entry. Mirrors the compiled contract.
	fn dispatch_ref(cpu: &mut Cpu, blocks: &[(u64, Vec<BlockOp>)], fuel: u64) -> u64 {
		let mut retired = 0u64;
		loop {
			let at = cpu.pc;
			let Some((slot, _)) = blocks.iter().enumerate().find(|(_, b)| b.0 == at) else {
				return retired;
			};
			if retired >= fuel {
				return retired;
			}
			retired += cpu.exec_block(slot);
			// slots were installed 1:1 with block order
		}
	}

	fn region_layout(cpu: &Cpu) -> jit::Layout {
		jit::Layout {
			x_base: XB, f_base: FB, tlb: None, pc_addr: PCA, gen_addr: GENA,
			baked_gen: cpu.mmu.code_gen(),
			dram_base: DB, guest_dram_base: DRAM_BASE, dram_len: WIN,
		}
	}

	fn run_region(bytes: &[u8], cpu_pre: &Cpu, entry: u32, fuel: u64) -> (u64, [i64; 32], u64) {
		let engine = wasmtime::Engine::default();
		let module = wasmtime::Module::new(&engine, bytes).expect("valid region module");
		let mut store = wasmtime::Store::new(&engine, ());
		let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
		{
			let d = mem.data_mut(&mut store);
			for i in 0..32 {
				d[XB as usize + i * 8..XB as usize + i * 8 + 8]
					.copy_from_slice(&cpu_pre.x[i].to_le_bytes());
			}
			d[GENA as usize..GENA as usize + 4]
				.copy_from_slice(&cpu_pre.mmu.code_gen().to_le_bytes());
			let mut win = vec![0u8; WIN as usize];
			cpu_pre.mmu.read_physical_range(DRAM_BASE, &mut win);
			d[DB as usize..DB as usize + WIN as usize].copy_from_slice(&win);
		}
		let instance = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
		let run = instance
			.get_typed_func::<(i64, i32), i64>(&mut store, "run")
			.unwrap();
		let retired = run.call(&mut store, (fuel as i64, entry as i32)).unwrap() as u64;
		let d = mem.data(&store);
		let mut x = [0i64; 32];
		for i in 0..32 {
			let mut b = [0u8; 8];
			b.copy_from_slice(&d[XB as usize + i * 8..XB as usize + i * 8 + 8]);
			x[i] = i64::from_le_bytes(b);
		}
		let mut b = [0u8; 8];
		b.copy_from_slice(&d[PCA as usize..PCA as usize + 8]);
		(retired, x, u64::from_le_bytes(b))
	}

	/// two-block counted loop: A does work and loops on itself via BNE,
	/// falls through to B, which stores the result and leaves the region
	fn loop_region() -> Vec<(u64, Vec<BlockOp>)> {
		let a = DRAM_BASE;
		let b = DRAM_BASE + 12;
		vec![
			(a, vec![
				op(HOT_ADD, 5, 5, 7, 0),      // x5 += x7
				op(HOT_ADDI, 6, 6, 0, -1),    // x6 -= 1
				op(HOT_BNE, 0, 6, 0, -8),     // while x6 != 0 -> A
			]),
			(b, vec![
				op(HOT_SD, 0, 10, 5, 0),      // [x10] = x5
				op(HOT_ADDI, 28, 28, 0, 99),
			]),
		]
	}

	#[test]
	fn region_loop_matches_dispatch() {
		for &(iters, fuel) in
			&[(1u64, 1u64 << 40), (7, 1 << 40), (1000, 1 << 40), (1000, 7), (1000, 1700), (5, 0)]
		{
			let blocks = loop_region();
			let mut cpu = fresh_cpu(&mut Rng(31337));
			cpu.x[6] = iters as i64;
			cpu.x[10] = (DRAM_BASE + 9000 & !7) as i64;
			let lay = region_layout(&cpu);
			let bytes = jit::emit_region(&blocks, &lay).expect("region emits");
			let (rw, xw, pcw) = run_region(&bytes, &cpu, 0, fuel);
			cpu.update_pc(blocks[0].0);
			for (slot, (start, ops)) in blocks.iter().enumerate() {
				cpu.install_block_for_test(slot, *start, 0, ops);
			}
			let ri = dispatch_ref(&mut cpu, &blocks, fuel);
			assert_eq!(ri, rw, "retired mismatch iters={} fuel={}", iters, fuel);
			assert_eq!(cpu.pc, pcw, "pc mismatch iters={} fuel={}", iters, fuel);
			assert_eq!(cpu.x, xw, "registers mismatch iters={} fuel={}", iters, fuel);
		}
	}

	#[test]
	fn tlb_tier_translates_hits_and_bails_misses() {
		// synthetic single-page TLB: virtual page V maps to physical page P
		// inside the DRAM window; everything else must bail.
		const SETS: u32 = 512;
		const T_RT: u32 = 8192; // read tags (512 * 8)
		const T_RM: u32 = 8192 + 4096; // read metas (512 * 4)
		const T_RP: u32 = 8192 + 4096 + 2048; // read ppns
		const T_MC: u32 = 8192 + 4096 + 2048 + 4096; // meta cache cell
		let vpage: u64 = 0x4000_2000; // arbitrary virtual page
		let ppage: u64 = DRAM_BASE + 0x3000; // physical page in DRAM
		let meta: u32 = 0xabcd_1234;

		let mut cpu = fresh_cpu(&mut Rng(4242));
		cpu.x[10] = (vpage + 0x40) as i64; // pointer into the mapped page
		cpu.x[11] = 0x5000_0000; // pointer with NO mapping
		let ops_hit = vec![op(HOT_LD, 5, 10, 0, 8), op(HOT_SD, 0, 10, 6, 16)];
		let ops_miss = vec![op(HOT_ADDI, 5, 5, 0, 1), op(HOT_LD, 7, 11, 0, 0)];
		let lay = jit::Layout {
			x_base: XB, f_base: FB,
			tlb: Some(jit::TlbLayout {
				sets: SETS,
				read_tags: T_RT, read_metas: T_RM, read_ppns: T_RP,
				// write set shares the arrays in this synthetic setup
				write_tags: T_RT, write_metas: T_RM, write_ppns: T_RP,
				meta_cache: T_MC,
			}),
			pc_addr: PCA, gen_addr: GENA,
			baked_gen: cpu.mmu.code_gen(),
			dram_base: DB, guest_dram_base: DRAM_BASE, dram_len: WIN,
		};
		let fill_tlb = |d: &mut [u8]| {
			let set = ((vpage >> 12) & (SETS as u64 - 1)) as usize;
			let tag = (vpage & !0xfff) | 1;
			d[T_RT as usize + set * 8..T_RT as usize + set * 8 + 8]
				.copy_from_slice(&tag.to_le_bytes());
			d[T_RM as usize + set * 4..T_RM as usize + set * 4 + 4]
				.copy_from_slice(&meta.to_le_bytes());
			d[T_RP as usize + set * 8..T_RP as usize + set * 8 + 8]
				.copy_from_slice(&(ppage & !0xfff).to_le_bytes());
			d[T_MC as usize..T_MC as usize + 4].copy_from_slice(&meta.to_le_bytes());
		};

		// hit case: LD then SD through the mapping run to completion
		let bytes = jit::emit_block(&ops_hit, DRAM_BASE, &lay).unwrap();
		let engine = wasmtime::Engine::default();
		let module = wasmtime::Module::new(&engine, &bytes).unwrap();
		let mut store = wasmtime::Store::new(&engine, ());
		let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
		{
			let d = mem.data_mut(&mut store);
			for i in 0..32 {
				d[XB as usize + i * 8..XB as usize + i * 8 + 8]
					.copy_from_slice(&cpu.x[i].to_le_bytes());
			}
			d[GENA as usize..GENA as usize + 4]
				.copy_from_slice(&cpu.mmu.code_gen().to_le_bytes());
			fill_tlb(d);
			// plant a known value at the translated load address
			let lin = DB as u64 + (ppage - DRAM_BASE) + 0x40 + 8;
			d[lin as usize..lin as usize + 8].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
		}
		let inst = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
		let run = inst.get_typed_func::<(), i64>(&mut store, "run").unwrap();
		let retired = run.call(&mut store, ()).unwrap();
		assert_eq!(retired, 2, "hit case must complete");
		{
			let d = mem.data(&store);
			let mut b = [0u8; 8];
			b.copy_from_slice(&d[XB as usize + 5 * 8..XB as usize + 5 * 8 + 8]);
			assert_eq!(u64::from_le_bytes(b), 0x1122_3344_5566_7788, "loaded through mapping");
			// the SD wrote x6 at translated +0x40+16
			let lin = DB as u64 + (ppage - DRAM_BASE) + 0x40 + 16;
			let mut b = [0u8; 8];
			b.copy_from_slice(&d[lin as usize..lin as usize + 8]);
			assert_eq!(i64::from_le_bytes(b), cpu.x[6], "stored through mapping");
		}

		// miss case: first op runs, the unmapped LD bails with pc at it
		let bytes = jit::emit_block(&ops_miss, DRAM_BASE, &lay).unwrap();
		let module = wasmtime::Module::new(&engine, &bytes).unwrap();
		let mut store = wasmtime::Store::new(&engine, ());
		let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
		{
			let d = mem.data_mut(&mut store);
			for i in 0..32 {
				d[XB as usize + i * 8..XB as usize + i * 8 + 8]
					.copy_from_slice(&cpu.x[i].to_le_bytes());
			}
			fill_tlb(d);
		}
		let inst = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
		let run = inst.get_typed_func::<(), i64>(&mut store, "run").unwrap();
		let retired = run.call(&mut store, ()).unwrap();
		assert_eq!(retired, 1, "miss bails before the load");
		{
			let d = mem.data(&store);
			let mut b = [0u8; 8];
			b.copy_from_slice(&d[PCA as usize..PCA as usize + 8]);
			assert_eq!(u64::from_le_bytes(b), DRAM_BASE + 4, "pc at the bailing op");
		}

		// stale meta: flip the cache cell; the mapped LD must now bail at op 0
		let bytes = jit::emit_block(&ops_hit, DRAM_BASE, &lay).unwrap();
		let module = wasmtime::Module::new(&engine, &bytes).unwrap();
		let mut store = wasmtime::Store::new(&engine, ());
		let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
		{
			let d = mem.data_mut(&mut store);
			for i in 0..32 {
				d[XB as usize + i * 8..XB as usize + i * 8 + 8]
					.copy_from_slice(&cpu.x[i].to_le_bytes());
			}
			fill_tlb(d);
			d[T_MC as usize..T_MC as usize + 4].copy_from_slice(&meta.wrapping_add(1).to_le_bytes());
		}
		let inst = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
		let run = inst.get_typed_func::<(), i64>(&mut store, "run").unwrap();
		let retired = run.call(&mut store, ()).unwrap();
		assert_eq!(retired, 0, "stale meta bails immediately");
	}

	#[test]
	fn store_bails_on_stale_generation() {
		let mut r = Rng(97);
		let cpu = fresh_cpu(&mut r);
		let ops = vec![
			BlockOp { imm: 0, word: 0, data: 0, kind: HOT_ADDI, rd: 5, rs1: 6, rs2: 0, len: 4, _pad: 0 },
			BlockOp { imm: 0, word: 0, data: 0, kind: HOT_SD, rd: 0, rs1: 10, rs2: 7, len: 4, _pad: 0 },
			BlockOp { imm: 0, word: 0, data: 0, kind: HOT_ADDI, rd: 8, rs1: 9, rs2: 0, len: 4, _pad: 0 },
		];
		let lay = jit::Layout {
			x_base: XB, f_base: FB, tlb: None, pc_addr: PCA, gen_addr: GENA,
			baked_gen: cpu.mmu.code_gen().wrapping_add(1), // stale on purpose
			dram_base: DB, guest_dram_base: DRAM_BASE, dram_len: WIN,
		};
		let bytes = jit::emit_block(&ops, DRAM_BASE, &lay).unwrap();
		let (retired, _x, pc, _d, _f) = run_wasm(&bytes, &cpu, DRAM_BASE);
		// the store executes, the gen check fires after it: 2 retired,
		// pc at the third op
		assert_eq!(retired, 2);
		assert_eq!(pc, DRAM_BASE + 8);
	}
}
