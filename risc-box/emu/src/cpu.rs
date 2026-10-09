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
// risc-box patch: a page whose code generation reached this (rewritten 3+ times while holding code) is treated as
// a guest JIT's code pool by region formation (see jit_form_pass)
#[cfg(feature = "codegen")]
const JIT_REWRITTEN_PAGE_GEN: u32 = 4;

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
	code_gen: u32, // mmu.code_gen() when last known valid (the cheap check)
	page_gen: u32, // mmu.page_gen(phys_page) at build time: still equal = the page was not written since
	glob_gen: u32 // mmu.glob_gen() at build time
}

impl BlockHead {
	const EMPTY: BlockHead = BlockHead { tag: 0, phys_page: 0, count: 0, code_gen: 0, page_gen: 0, glob_gen: 0 };
}

// risc-box patch: 128k slots (was 32k). Direct-mapped by (pc >> 1), 32k slots alias every 64 KiB of code, and a browser's
// hot code spans megabytes: blocks evicted by aliasing are re-fetched and re-decoded on the next visit.
const BLOCK_SLOTS: usize = 0x20000; // 128k x (24B + 32x16B) = 67 MiB
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
	// risc-box patch: instructions actually EXECUTED. `clock` is guest TIME and
	// deliberately charges a WFI burst as if it had run, which is what keeps the
	// instruction-driven mtime advancing while the hart is parked. That makes it
	// useless for "is this guest getting anywhere", so the honest count is kept
	// separately - the app reports this one.
	retired: u64,
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
	/// risc-box patch: blocks decoded into the cache (a miss each), for /status
	block_builds: u64,
	// risc-box patch (tier2 feature): the region dispatcher in coverage
	// mode — forms regions from live heat/edges, "compiles" them into a
	// recording backend, and the run loop below counts the retired
	// instructions that would have executed as compiled code.
	#[cfg(feature = "tier2")]
	tier2: Option<Box<Tier2State>>,
	// risc-box patch (aot): hash-free dispatch mirrors. aot_slots is
	// direct-mapped by the SAME index as block_heads (one extra tag
	// compare per dispatch); aot_vstate is indexed by baked handle.
	// Synced from Tier2's maps once per formation pass.
	#[cfg(feature = "aot")]
	aot_slots: Vec<AotSlot>,
	#[cfg(feature = "aot")]
	aot_vstate: Vec<AotVerify>,
	/// installs that verified / candidate lookups that found no verifiable
	/// region — the two numbers that say whether the bake ENGAGES.
	#[cfg(feature = "aot")]
	aot_install_ok: u64,
	#[cfg(feature = "aot")]
	aot_install_fail: u64,
	// risc-box patch (codegen): the live region JIT over the platform's
	// enclave:codegen verb (see JitState).
	#[cfg(feature = "codegen")]
	jit: Option<Box<JitState>>,
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
	// risc-box patch: instructions between device services (DEVICE_TICK_INTERVAL unless the app sets it). Device
	// clocks advance by the true retired count either way; this only bounds how often they are serviced.
	tick_interval: u64,
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
	fn compile(&mut self, _module: &[u8], _entry_pcs: &[u64]) -> Option<u32> {
		None // keying happens in compile_src, on the blocks
	}
	fn compile_src(
		&mut self,
		blocks: &[(u64, Vec<BlockOp>)],
		_module: &[u8],
		_entry_pcs: &[u64],
	) -> Option<u32> {
		let h = ::jit::hash_blocks(blocks);
		AOT_HASHES.iter().position(|&x| x == h).map(|i| i as u32)
	}
	fn call(&mut self, _h: u32, _fuel: u64, _entry: u32) -> u64 {
		0
	}
	fn drop_region(&mut self, _h: u32) {}
}

#[cfg(feature = "aot")]
#[derive(Clone, Copy)]
pub struct AotSlot {
	tag: u64, // entry pc (0 = empty)
	handle: u32,
	entry: u32,
	gen: u32, // code generation at install
}

#[cfg(feature = "aot")]
impl AotSlot {
	const EMPTY: AotSlot = AotSlot { tag: 0, handle: 0, entry: 0, gen: 0 };
}

#[cfg(feature = "aot")]
#[derive(Clone)]
pub struct AotVerify {
	/// code generation of the last full content check (0 = never)
	content_gen: u32,
	/// address-space generation the mapping check last passed under
	tlb_gen: u32,
	/// pages visited by the content check; a proof only when `ok` is true
	phys: Vec<u64>,
	/// risc-box patch: each visited page's code generation at that check, and the whole-memory generation:
	/// while they all still match, the content proof stands through epoch bumps from stores to other pages
	pgens: Vec<u32>,
	glob: u32,
	ok: bool,
}

#[cfg(feature = "aot")]
impl AotVerify {
	const NEVER: AotVerify = AotVerify {
		content_gen: 0,
		tlb_gen: 0,
		phys: Vec::new(),
		pgens: Vec::new(),
		glob: 0,
		ok: false,
	};
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
	/// This dispatcher's handles index the BAKED tables (aot_enable set it
	/// up). The run loop must never treat a recording backend's handles as
	/// baked-function indexes — same u32, entirely different meaning.
	pub aot: bool,
	/// Sampled heat of UNCOVERED dispatches — where the mass a bake is not
	/// reaching actually lives (read via tier2_miss_top at run end).
	pub miss: std::collections::HashMap<u64, u64>,
}

/// risc-box patch (codegen): the live region JIT. Hot regions are formed
/// from sampled block heat (the same greedy formation the AOT bake uses),
/// emitted by jit::emit_region against the machine's real layout, compiled
/// through the platform verb (jit::verb owns the process-wide budget), and
/// dispatched from the run loop exactly where the AOT splice runs baked
/// regions: same fuel-bounded call, same interrupt/device-service cadence.
///
/// Validity is the AOT's two-level proof, kept per installed instance:
/// content (each member's (word, len) stream really is in memory) is proven
/// once per write-snoop generation, and the proof marks every member page
/// executable — so any later store to them bumps the generation and forces
/// a re-proof; mapping (each member pc still fetches from the page the proof
/// saw) is re-probed once per TLB meta (address-space generation, privilege,
/// MPRV/MPP). A region therefore survives generation bumps elsewhere in the
/// machine without recompiling, and a region whose code changed or whose
/// pages are not mapped here simply does not run.
#[cfg(feature = "codegen")]
pub struct JitState {
	t2: ::jit::Tier2,
	lay: ::jit::Layout,
	lay_hash: u64,
	/// direct-mapped by the same index as block_heads
	slots: Vec<JitSlot>,
	/// one bit per slot ever installed: the dispatch path tests this 4 KiB
	/// (L1-resident) map before touching the 512 KiB slot array
	present: Vec<u64>,
	regions: Vec<JitRegion>,
	free: Vec<u32>,
	/// (table index, pc bias, entry base) -> installed instance: a packed
	/// module holds several regions, which may share a bias
	instances: ::fnv::FnvHashMap<(u64, u64, u32), u32>,
	/// block pcs whose code changed under a proven region (self-modifying
	/// or JIT-generated guest code): never formed into a region again, so
	/// the compile budget is not spent chasing code that keeps changing
	volatile: ::fnv::FnvHashSet<u64>,
	/// the execution view whose table holds our indices
	owner: usize,
	/// this run() may dispatch: owner thread, RV64
	live: bool,
	/// when the JIT was enabled (compile-time share cap)
	since: std::time::Instant,
	/// sampling window over interpreted retirement (see record)
	total: u64,
	window: bool,
	pub params: JitParams,
	pub stats: JitStats,
	diag: JitDiag,
}

/// Tuning, defaulted for the desktop/browser workloads.
#[cfg(feature = "codegen")]
#[derive(Clone, Debug)]
pub struct JitParams {
	/// instructions a region call may run before returning at a block
	/// boundary (the AOT splice's 256: interrupt jitter near a block's)
	pub fuel: u64,
	/// retired instructions between formation passes
	pub form_interval: u64,
	/// sampled heat a block needs to seed a region
	pub seed_heat: u64,
	/// sampled region heat that justifies a compile (escalated by the verb
	/// policy as the budget is spent)...
	pub compile_heat: u64,
	/// ...and at least this much per guest op in the region: compile time is
	/// linear in size (~0.07 ms per op measured in the app), so a large,
	/// lukewarm region must show proportionally more heat to pay back
	pub compile_heat_per_op: u64,
	/// members colder than this (sampled) are dropped before emission
	pub prune_heat: u64,
	pub max_blocks: usize,
	pub max_compiles_per_pass: u32,
	/// log2 of the sampling window (retired instructions) and the duty
	/// cycle: one window in `1 << sample_period` is recorded
	pub sample_shift: u32,
	pub sample_period: u32,
	/// largest module submitted
	pub max_module_bytes: usize,
	/// the app memory's declared maximum, in 64 KiB pages
	pub max_pages: u64,
	/// one stderr line per formation pass
	pub trace: bool,
	/// compile only while cumulative compile time stays under this share of
	/// the wall time since the JIT was enabled (percent, plus a 250 ms
	/// allowance); 100 = no cap. Compiles stall the machine's own thread.
	pub compile_pct: u32,
	/// risc-box patch: when a region returns at a pc that starts another proven region, call that one directly
	/// (within the same fuel) instead of going back through the block cache probe
	pub chain: bool,
}

#[cfg(feature = "codegen")]
impl Default for JitParams {
	fn default() -> JitParams {
		JitParams {
			fuel: 256,
			form_interval: 50_000_000,
			// sampled heat: 1 window of 2^18 retired in 2^6 is recorded, so
			// these are 1/64 of the true counts per 50M-instruction pass
			// (seed ~0.25%, compile ~0.8% of the pass, before escalation)
			seed_heat: 2_000,
			compile_heat: 6_000,
			compile_heat_per_op: 20,
			prune_heat: 256,
			max_blocks: 64,
			max_compiles_per_pass: 4,
			sample_shift: 18,
			sample_period: 6,
			max_module_bytes: 128 * 1024,
			max_pages: 1 << 18, // --max-memory=17179869184
			trace: false,
			compile_pct: 100,
			chain: true,
		}
	}
}

#[cfg(feature = "codegen")]
#[derive(Clone, Debug, Default)]
pub struct JitStats {
	/// region calls that ran, instructions they retired, calls that ran nothing
	pub calls: u64,
	pub retired: u64,
	pub empty_calls: u64,
	/// region calls made straight from a previous region's exit (no dispatcher round trip in between)
	pub chained: u64,
	/// formation members left out because their page keeps being rewritten (guest JIT code)
	pub skipped_rewritten: u64,
	/// interpreted block retirement seen while the JIT was on
	pub interpreted: u64,
	pub content_checks: u64,
	pub map_checks: u64,
	pub verify_failures: u64,
	pub passes: u64,
	pub formed: u64,
	pub installs: u64,
	pub refused: u64,
	pub oversize: u64,
	pub live_regions: u64,
	pub resets: u64,
	/// blocks excluded from formation because their code changed
	pub volatile: u64,
	/// time spent forming (and emitting/compiling) regions, microseconds
	pub form_us: u64,
	/// risc-box patch (packed modules): modules compiled from packs, regions placed in them, and regions installed
	/// from the process-wide placement cache (re-formed, or formed by another machine) without a compile
	pub packs: u64,
	pub packed_regions: u64,
	pub pack_reused: u64,
}

#[cfg(feature = "codegen")]
#[derive(Clone, Copy)]
struct JitSlot {
	tag: u64, // entry pc (0 = empty)
	region: u32,
	entry: u32,
}

#[cfg(feature = "codegen")]
impl JitSlot {
	const EMPTY: JitSlot = JitSlot { tag: 0, region: 0, entry: 0 };
}

/// One compiled region placed at a page-aligned pc bias: a module of its
/// own, or one group of a packed module (entries entry_base..+members).
#[cfg(feature = "codegen")]
struct JitRegion {
	index: u64,
	bias: u64,
	entry_base: u32,
	/// runtime start pc of each member block, in module block order, with
	/// the (uncompressed word, length) stream the module was built from
	members: Vec<(u64, Vec<(u32, u8)>)>,
	/// (virtual page, physical page) of every page the members start on,
	/// from the last content proof: what the mapping re-probe checks
	pages: Vec<(u64, u64)>,
	/// write-snoop generation of the last successful content proof (0: none)
	proof_gen: u32,
	/// per-page code generation of each `pages` entry at that proof, and the whole-memory generation: while
	/// they all still match, the content proof stands through epoch bumps caused by stores to OTHER pages
	page_gens: Vec<u32>,
	proof_glob: u32,
	/// a second proven (pages, page_gens, glob): the same code proven in another address space, so switching
	/// between two processes re-probes mappings instead of re-reading every member word each time
	alt: Option<(Vec<(u64, u64)>, Vec<u32>, u32)>,
	/// (generation, TLB meta) of the last check, and its verdict — a failed
	/// check is cached too, so a region that cannot run here costs one
	/// compare per dispatch, not a proof
	checked: (u32, u32),
	ok: bool,
	refs: u32, // slots pointing here
	/// risc-box patch (diag): calls into this instance, instructions they retired, calls that ran nothing
	calls: u64,
	retired: u64,
	empty: u64,
}

/// risc-box patch (diag): where region calls end, sampled one call in 64. `kinds`: [at a member block's start (fuel or a
/// bail at the block's first op), inside a member block (a bail mid-block: TLB miss, MMIO, untranslated op),
/// outside every member (control left the region)]; `outside`/`bails` count those pcs.
#[cfg(feature = "codegen")]
#[derive(Default)]
struct JitDiag {
	kinds: [u64; 3],
	outside: ::fnv::FnvHashMap<u64, u64>,
	bails: ::fnv::FnvHashMap<u64, u64>,
}

#[cfg(feature = "codegen")]
impl JitState {
	/// Sampled recording of one interpreted block: heat and successor edges
	/// in one window of `1 << sample_period`, the formation clock always.
	#[inline(always)]
	fn record(&mut self, tag: u64, r: u64) {
		self.total += r;
		self.stats.interpreted += r;
		let w = (self.total >> self.params.sample_shift) & ((1 << self.params.sample_period) - 1) == 0;
		if w != self.window {
			if w {
				// never chain an edge across the unrecorded gap
				self.t2.note_break();
			}
			self.window = w;
		}
		match w {
			true => self.t2.note_block(tag, r),
			false => self.t2.note_retire(r),
		}
	}

	/// Point `slot` at region `rid`, releasing whatever it held.
	fn install(&mut self, slot: usize, tag: u64, rid: u32, entry: u32) {
		let old = self.slots[slot];
		// referenced before the old slot is released: re-pointing a slot
		// within one region must never free that region
		self.regions[rid as usize].refs += 1;
		if old.tag != 0 {
			self.release(old.region);
		}
		self.slots[slot] = JitSlot { tag, region: rid, entry };
		self.present[slot >> 6] |= 1 << (slot & 63);
	}

	fn release(&mut self, rid: u32) {
		let r = &mut self.regions[rid as usize];
		r.refs -= 1;
		if r.refs == 0 {
			self.instances.remove(&(r.index, r.bias, r.entry_base));
			r.members = Vec::new();
			r.pages = Vec::new();
			r.page_gens = Vec::new();
			r.proof_glob = 0;
			r.alt = None;
			r.ok = false;
			r.proof_gen = 0;
			r.checked = (0, 0);
			self.free.push(rid);
			self.stats.live_regions -= 1;
		}
	}

	/// An instance of the region at entry `base` of compiled `index`, at
	/// `bias` (shared with an identical one already installed).
	fn instance(&mut self, index: u64, bias: u64, base: u32, members: Vec<(u64, Vec<(u32, u8)>)>) -> u32 {
		if let Some(&rid) = self.instances.get(&(index, bias, base)) {
			return rid;
		}
		let r = JitRegion {
			index, bias, entry_base: base, members, pages: Vec::new(), proof_gen: 0, page_gens: Vec::new(),
			proof_glob: 0, alt: None, checked: (0, 0), ok: false, refs: 0, calls: 0, retired: 0, empty: 0,
		};
		let rid = match self.free.pop() {
			Some(rid) => {
				self.regions[rid as usize] = r;
				rid
			}
			None => {
				self.regions.push(r);
				(self.regions.len() - 1) as u32
			}
		};
		self.instances.insert((index, bias, base), rid);
		self.stats.live_regions += 1;
		rid
	}

	/// risc-box patch (packed modules): install a region compiled at entry `base` of module `index`: its own
	/// instance (own bias, own members, own proof), every member slot pointing at base + i.
	fn place(&mut self, blocks: &[(u64, u64, Vec<BlockOp>)], index: u64, bias: u64, base: u32) {
		// blocks are in pc order here: the region's block order
		let words: Vec<(u64, Vec<(u32, u8)>)> =
			blocks.iter().map(|b| (b.0, b.2.iter().map(|o| (o.word, o.len)).collect())).collect();
		let rid = self.instance(index, bias, base, words);
		for (i, b) in blocks.iter().enumerate() {
			let slot = ((b.0 >> 1) as usize) & (BLOCK_SLOTS - 1);
			self.install(slot, b.0, rid, base + i as u32);
		}
		self.stats.installs += 1;
	}

	/// Compile time is capped at a share of the wall time since the JIT was enabled (JitParams::compile_pct).
	fn may_compile(&self) -> bool {
		let spent_ms = ::jit::verb::stats().compile_us / 1000;
		let wall_ms = self.since.elapsed().as_millis() as u64;
		spent_ms <= 250 + wall_ms * self.params.compile_pct as u64 / 100
	}

	/// risc-box patch (diag): classify one sampled region exit (see JitDiag).
	fn diag_exit(&mut self, rid: u32, pc: u64) {
		let mut kind = 2;
		for (start, words) in self.regions[rid as usize].members.iter() {
			let len: u64 = words.iter().map(|w| w.1 as u64).sum();
			if pc == *start {
				kind = 0;
				break;
			}
			if pc > *start && pc < start + len {
				kind = 1;
				break;
			}
		}
		let d = &mut self.diag;
		d.kinds[kind] += 1;
		let map = match kind {
			2 => &mut d.outside,
			1 => &mut d.bails,
			_ => return,
		};
		if map.len() < 1 << 14 || map.contains_key(&pc) {
			*map.entry(pc).or_insert(0) += 1;
		}
	}

	/// risc-box patch (diag): the hottest instances by calls, the exit classes and the commonest exit pcs, as JSON.
	fn diag_json(&self) -> String {
		let mut regs: Vec<&JitRegion> = self.regions.iter().filter(|r| r.calls > 0).collect();
		regs.sort_by(|a, b| b.calls.cmp(&a.calls));
		let top: Vec<String> = regs.iter().take(24).map(|r| {
			let ops: usize = r.members.iter().map(|m| m.1.len()).sum();
			format!("[\"{:#x}\",{},{},{},{},{}]", r.members.first().map_or(0, |m| m.0), r.members.len(), ops, r.calls, r.retired, r.empty)
		}).collect();
		let hist = |m: &::fnv::FnvHashMap<u64, u64>| {
			let mut v: Vec<(u64, u64)> = m.iter().map(|(&k, &n)| (k, n)).collect();
			v.sort_by(|a, b| b.1.cmp(&a.1));
			let tot: u64 = v.iter().map(|e| e.1).sum();
			let s: Vec<String> = v.iter().take(30).map(|&(pc, n)| {
				let slot = ((pc >> 1) as usize) & (BLOCK_SLOTS - 1);
				let entry = self.slots[slot].tag == pc;
				format!("[\"{:#x}\",{},{}]", pc, n, entry as u8)
			}).collect();
			format!("{{\"distinct\":{},\"total\":{},\"top\":[{}]}}", v.len(), tot, s.join(","))
		};
		format!("{{\"kinds\":[{},{},{}],\"regions\":[{}],\"outside\":{},\"bails\":{}}}",
			self.diag.kinds[0], self.diag.kinds[1], self.diag.kinds[2], top.join(","), hist(&self.diag.outside), hist(&self.diag.bails))
	}

	/// Drop every installed instance (the layout they were placed under no
	/// longer holds). Compiled modules stay cached in jit::verb.
	fn clear(&mut self) {
		for s in self.slots.iter_mut() {
			*s = JitSlot::EMPTY;
		}
		for w in self.present.iter_mut() {
			*w = 0;
		}
		self.regions.clear();
		self.free.clear();
		self.instances.clear();
		self.stats.live_regions = 0;
		self.stats.resets += 1;
	}

	/// risc-box patch (packed modules): give a candidate its group code, emitted once. A region too large for any
	/// module is cut to its hotter half once, as a region too large to compile always was (a half placed before
	/// is installed right away). False: the candidate is settled (installed, or dropped and never submitted
	/// again) and leaves the pass.
	fn emit_cand(&mut self, c: &mut PackCand, max_module: usize) -> bool {
		if c.code.is_some() {
			return true;
		}
		loop {
			let emitted = match ::jit::emit_group(&c.rel(), &self.lay) {
				Some(g) if g.pack_cost() + ::jit::PACK_FIXED_BOUND <= max_module => {
					c.code = Some(g);
					return true;
				}
				Some(_) => true,
				None => false,
			};
			if emitted {
				self.stats.oversize += 1;
			}
			::jit::verb::place(c.key, None);
			c.done = true;
			if !emitted || c.split || c.blocks.len() < 2 {
				return false;
			}
			// keep the hotter half
			let mut blocks = std::mem::take(&mut c.blocks);
			blocks.sort_by(|a, b| b.1.cmp(&a.1));
			blocks.truncate((blocks.len() + 1) / 2);
			*c = PackCand::new(blocks, &self.params, self.lay_hash);
			c.split = true;
			match ::jit::verb::placed(c.key) {
				Some(Some((index, base, n))) if n as usize == c.blocks.len() => {
					self.place(&c.blocks, index, c.bias, base);
					self.stats.pack_reused += 1;
					c.done = true;
					c.installed = true;
					return false;
				}
				Some(_) => {
					c.done = true;
					return false;
				}
				None => {}
			}
		}
	}

	/// risc-box patch (packed modules): compile the formed regions no module holds yet, SEVERAL PER MODULE. The
	/// host's module quota (256 per process, never refunded) used to run out with a third of its byte quota
	/// spent; a pack moves the limit to bytes. Hottest region first, a module is filled with every waiting region
	/// (in heat order) that keeps the module's total heat at or above its total need times the verb's current
	/// escalation, and that still fits under the size cap and the bytes left; it is then submitted ONCE, so the
	/// verb's budget and heat escalation apply per module exactly as they applied per region. Each region in
	/// it becomes its own instance with entries base + i and is remembered process-wide (verb::placed). A
	/// region never rides along below its own unescalated need: compile time per op still has to pay back.
	/// Returns how many regions were installed.
	fn compile_packs(&mut self, mut left: Vec<PackCand>) -> u64 {
		let mut installed = 0u64;
		let (mut refused_heat, mut refused_budget) = (0u64, 0u64);
		left.sort_by(|a, b| b.heat.cmp(&a.heat).then(a.key.cmp(&b.key)));
		let cold = left.iter().filter(|c| c.heat < c.need).count() as u64;
		left.retain(|c| c.heat >= c.need);
		refused_heat += cold;
		let mut compiles = 0u32;
		while !left.is_empty() && compiles < self.params.max_compiles_per_pass && self.may_compile() {
			let bar = match ::jit::verb::bar() {
				Some(b) => b,
				None => {
					// the module or attempt budget is spent (or compiling is off for good)
					if ::jit::verb::stats().disabled.is_none() {
						refused_budget += left.len() as u64;
					}
					left.clear();
					break;
				}
			};
			let max_module = self.params.max_module_bytes.min(bar.max_module_bytes);
			let limit = (max_module as u64).min(bar.bytes_left) as usize;
			let fits_bar = |h: u64, n: u64| h >= n.saturating_mul(bar.heat_mult);
			// fill: greedy in heat order, against a bound of the module's size
			let mut pick: Vec<usize> = Vec::new();
			let (mut heat, mut need, mut cost) = (0u64, 0u64, ::jit::PACK_FIXED_BOUND);
			for i in 0..left.len() {
				if pick.len() >= ::jit::MAX_PACK_GROUPS {
					break;
				}
				if !fits_bar(heat + left[i].heat, need + left[i].need) {
					continue;
				}
				if !self.emit_cand(&mut left[i], max_module) {
					installed += left[i].installed as u64;
					continue;
				}
				// a split candidate's heat and need moved: ask again
				let (h, n) = (left[i].heat, left[i].need);
				let add = left[i].code.as_ref().map_or(usize::MAX, |g| g.pack_cost());
				if !fits_bar(heat + h, need + n) || cost.saturating_add(add) > limit {
					continue;
				}
				pick.push(i);
				heat += h;
				need += n;
				cost += add;
			}
			if pick.is_empty() {
				// nothing waiting clears the bar (alone or with others), or fits the bytes left
				for c in left.iter().filter(|c| !c.done) {
					match fits_bar(c.heat, c.need) {
						true => refused_budget += 1,
						false => refused_heat += 1,
					}
				}
				left.clear();
				break;
			}
			// exact size: the bound keeps the cap from binding here; should it ever, the coldest group goes
			let mut built = None;
			while let Some(&last) = pick.last() {
				let groups: Vec<&::jit::GroupCode> = pick.iter().map(|&i| left[i].code.as_ref().unwrap()).collect();
				match ::jit::pack(&groups, &self.lay) {
					Some((m, b)) if m.len() <= limit => {
						built = Some((m, b));
						break;
					}
					_ => {
						pick.pop();
						if pick.is_empty() {
							::jit::verb::place(left[last].key, None);
							left[last].done = true;
							self.stats.oversize += 1;
						}
					}
				}
			}
			let (module, bases) = match built {
				Some(b) => b,
				None => {
					left.retain(|c| !c.done);
					continue;
				}
			};
			let keys: Vec<(u64, u64, u64)> = pick.iter().map(|&i| left[i].key).collect();
			let heat: u64 = pick.iter().map(|&i| left[i].heat).sum();
			let need: u64 = pick.iter().map(|&i| left[i].need).sum();
			let size = module.len();
			let t0 = std::time::Instant::now();
			let r = ::jit::verb::lookup(::jit::pack_key(&keys), heat, need, true, move || Some(module));
			if self.params.trace {
				let blocks: usize = pick.iter().map(|&i| left[i].blocks.len()).sum();
				let ops: usize = pick.iter().map(|&i| left[i].blocks.iter().map(|b| b.2.len()).sum::<usize>()).sum();
				eprintln!(
					"[jit] pack {:?}: {} regions, {} blocks, {} ops, {} bytes, heat {} (need {} x{}), {:.1} ms",
					r, pick.len(), blocks, ops, size, heat, need, bar.heat_mult, t0.elapsed().as_secs_f64() * 1000.0
				);
			}
			match r {
				::jit::verb::Got::Compiled(index) | ::jit::verb::Got::Cached(index) => {
					for (k, &i) in pick.iter().enumerate() {
						let c = &mut left[i];
						::jit::verb::place(c.key, Some((index, bases[k], c.blocks.len() as u32)));
						c.done = true;
						let (blocks, bias) = (std::mem::take(&mut c.blocks), c.bias);
						self.place(&blocks, index, bias, bases[k]);
					}
					installed += pick.len() as u64;
					self.stats.packed_regions += pick.len() as u64;
					if matches!(r, ::jit::verb::Got::Compiled(_)) {
						self.stats.packs += 1;
						compiles += 1;
					}
				}
				::jit::verb::Got::Failed => {
					// a failed module is never resubmitted, nor are its regions
					compiles += 1;
					for &i in pick.iter() {
						::jit::verb::place(left[i].key, None);
						left[i].done = true;
					}
				}
				// Neither happens while bar() describes the verb (the pack was sized and filled against it); either
				// way these regions wait for a later pass, and the pack's key stays refused in the verb
				::jit::verb::Got::TooLarge | ::jit::verb::Got::Refused => {
					if r == ::jit::verb::Got::TooLarge {
						self.stats.oversize += 1;
					}
					for &i in pick.iter() {
						left[i].done = true;
					}
				}
			}
			left.retain(|c| !c.done);
		}
		::jit::verb::note_refused(refused_heat, refused_budget);
		installed
	}
}

/// risc-box patch (packed modules): a formed region waiting for a module.
#[cfg(feature = "codegen")]
struct PackCand {
	/// (runtime pc, sampled heat, ops), in pc order: the region's block order
	blocks: Vec<(u64, u64, Vec<BlockOp>)>,
	bias: u64,
	/// source key of the region alone (position independent: module pcs)
	key: (u64, u64, u64),
	heat: u64,
	/// max(compile_heat, compile_heat_per_op * ops): never escalated per region
	need: u64,
	code: Option<::jit::GroupCode>,
	/// already cut to its hotter half (too large for any module)
	split: bool,
	/// settled this pass: installed, or dropped
	done: bool,
	installed: bool,
}

#[cfg(feature = "codegen")]
impl PackCand {
	fn new(mut blocks: Vec<(u64, u64, Vec<BlockOp>)>, params: &JitParams, lay_hash: u64) -> PackCand {
		blocks.sort_by_key(|b| b.0);
		let bias = blocks[0].0 & !0xfff;
		let rel: Vec<(u64, Vec<BlockOp>)> = blocks.iter().map(|b| (b.0 - bias, b.2.clone())).collect();
		let key = ::jit::source_key(&rel, lay_hash);
		let heat = blocks.iter().map(|b| b.1).sum();
		let ops: u64 = blocks.iter().map(|b| b.2.len() as u64).sum();
		let need = params.compile_heat.max(params.compile_heat_per_op * ops);
		PackCand { blocks, bias, key, heat, need, code: None, split: false, done: false, installed: false }
	}

	/// The blocks at module pcs (runtime pc - bias): what the module is built from.
	fn rel(&self) -> Vec<(u64, Vec<BlockOp>)> {
		self.blocks.iter().map(|b| (b.0 - self.bias, b.2.clone())).collect()
	}
}

impl Cpu {
	/// Creates a new `Cpu`.
	///
	/// # Arguments
	/// * `Terminal`
	pub fn new(terminal: Box<dyn Terminal>) -> Self {
		let mut cpu = Cpu {
			clock: 0,
			retired: 0,
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
			block_builds: 0,
			#[cfg(feature = "tier2")]
			tier2: None,
			#[cfg(feature = "aot")]
			aot_slots: vec![AotSlot::EMPTY; BLOCK_SLOTS],
			#[cfg(feature = "aot")]
			aot_vstate: Vec::new(),
			#[cfg(feature = "aot")]
			aot_install_ok: 0,
			#[cfg(feature = "aot")]
			aot_install_fail: 0,
			#[cfg(feature = "codegen")]
			jit: None,
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
			tick_interval: DEVICE_TICK_INTERVAL,
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

	// risc-box patch: instructions this hart has actually executed, idle time
	// excluded. Not carried in a snapshot - it is a measure of this run.
	pub fn retired(&self) -> u64 {
		self.retired
	}

	/// risc-box patch (diagnosis): (mtime, mtimecmp, wall_clock).
	pub fn timer_diag(&self) -> (u64, u64, bool) {
		let c = self.mmu.get_clint();
		(c.read_mtime(), c.read_mtimecmp(), c.is_wall())
	}

	/// risc-box patch (diagnosis): the supervisor state you need to tell a
	/// working guest from one going round a trap. Everything here is already
	/// in the CSR file; it is private, and without it an embedder can only
	/// watch a machine be inert from the outside.
	/// Returns (pc, mode, satp, scause, stval, sepc, stvec, sstatus, mip, mie).
	pub fn diag(&self) -> (u64, u8, u64, u64, u64, u64, u64, u64, u64, u64) {
		(
			self.pc,
			match self.privilege_mode { PrivilegeMode::User => 0, PrivilegeMode::Supervisor => 1,
			                            PrivilegeMode::Reserved => 2, PrivilegeMode::Machine => 3 },
			self.read_csr_raw(CSR_SATP_ADDRESS),
			self.read_csr_raw(CSR_SCAUSE_ADDRESS),
			self.read_csr_raw(CSR_STVAL_ADDRESS),
			self.read_csr_raw(CSR_SEPC_ADDRESS),
			self.read_csr_raw(CSR_STVEC_ADDRESS),
			self.read_csr_raw(CSR_SSTATUS_ADDRESS),
			self.read_csr_raw(CSR_MIP_ADDRESS),
			self.read_csr_raw(CSR_MIE_ADDRESS),
		)
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
	/// risc-box patch: instructions between device services (default DEVICE_TICK_INTERVAL = 32). Larger values
	/// cut the per-instruction share of servicing the clint/plic/virtio devices; device clocks still advance by
	/// the true retired count, so only interrupt-delivery latency changes (by at most the interval).
	pub fn set_device_tick_interval(&mut self, n: u64) {
		self.tick_interval = n.clamp(8, 4096);
	}

	/// risc-box patch (per-page code generations): the code epoch moved since block `slot` was last known valid;
	/// it still is if its own page was not written (and memory was not wholesale invalidated) since it was built.
	/// Valid: re-stamp it with the current epoch so the cheap check passes again.
	#[inline(always)]
	fn head_revalidate(&mut self, slot: usize) -> bool {
		let h = self.block_heads[slot];
		if h.tag != 0 && h.glob_gen == self.mmu.glob_gen() && h.page_gen == self.mmu.page_gen(h.phys_page) {
			self.block_heads[slot].code_gen = self.mmu.code_gen();
			return true;
		}
		false
	}

	/// The same test without re-stamping, for read-only callers.
	#[inline(always)]
	fn head_valid(&self, h: &BlockHead) -> bool {
		h.code_gen == self.mmu.code_gen()
			|| (h.tag != 0 && h.glob_gen == self.mmu.glob_gen() && h.page_gen == self.mmu.page_gen(h.phys_page))
	}

	pub fn run(&mut self, n: u64) {
		#[cfg(feature = "codegen")]
		self.jit_prepare();
		let mut remaining = n;
		// Superblocks retire several instructions per dispatch, so a call
		// stepping fewer instructions than a block might hold has to stay on
		// the single-instruction path — tick()/run(1) keeps exact stepping
		// for the tests and boot-bench's tracer.
		let allow_blocks = n >= BLOCK_MAX as u64;
		while remaining > 0 {
			// since_service < DEVICE_TICK_INTERVAL here (the service block
			// below resets it), so every burst makes progress.
			let until_service = self.tick_interval.saturating_sub(self.since_service).max(1);
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
			// Whatever the WFI branch just charged is idle time, not execution.
			let idle_charged = done;
			while done < burst {
				if allow_blocks {
					let slot = ((self.pc >> 1) as usize) & (BLOCK_SLOTS - 1);
					let h = self.block_heads[slot];
					let hit = h.tag == self.pc
						&& (h.code_gen == self.mmu.code_gen() || self.head_revalidate(slot))
						&& match self.mmu.translate_fetch_probe(self.pc) {
							Ok(p) => (p & !0xfff) == h.phys_page,
							Err(_) => false
						};
					if hit {
						// Coverage-only builds probe Tier2's map per
						// dispatch; the AOT build's hot path must not touch
						// a SipHash HashMap (65% of the first AOT profile).
						#[cfg(all(feature = "tier2", not(feature = "aot")))]
						let compiled = {
							let g = self.mmu.code_gen();
							match self.tier2.as_mut() {
								Some(t) => t.t2.lookup(h.tag, g),
								None => None,
							}
						};
						#[cfg(feature = "aot")]
						let compiled: Option<(u32, u32)> = {
							let sl = self.aot_slots[slot];
							// risc-box patch: an epoch bump from a store to some OTHER page keeps the slot (its
							// proof's pages are checked by generation, cheaply); re-stamp it when it still holds
							if sl.tag == h.tag && sl.gen != self.mmu.code_gen() && self.aot_verified(sl.handle) {
								self.aot_slots[slot].gen = self.mmu.code_gen();
							}
							let sl = self.aot_slots[slot];
							match sl.tag == h.tag && sl.gen == self.mmu.code_gen() {
								true => Some((sl.handle, sl.entry)),
								false => None,
							}
						};
						#[cfg(feature = "aot")]
						if let Some((hh, idx)) = compiled.filter(|&(hh, _)| self.aot_verified(hh)) {
							let g = self.mmu.code_gen();
							// Regions overshoot the service boundary the same
							// way blocks do (device clocks advance by TRUE
							// retired count); 256 keeps interrupt-delivery
							// jitter close to the block-sized overshoot while
							// letting a hot loop actually stay compiled.
							let fuel = 256u64;
							let ran = AOT_FNS[hh as usize](self, fuel, idx, g);
							if ran > 0 {
								if let Some(t) = self.tier2.as_mut() {
									t.total += ran;
									t.covered += ran;
									t.t2.note_break();
									t.t2.note_retire(ran);
								}
								done += ran;
								if self.check_interrupt {
									self.check_interrupt = false;
									self.handle_interrupt(self.pc);
								}
								continue;
							}
						}
						// The live JIT: a verified compiled region entered at
						// this block, under the same fuel/cadence contract.
						#[cfg(feature = "codegen")]
						{
							let ran = self.jit_run(slot, h.tag);
							if ran > 0 {
								#[cfg(feature = "tier2")]
								if let Some(t) = self.tier2.as_mut() {
									t.total += ran;
									t.t2.note_break();
									t.t2.note_retire(ran);
								}
								if let Some(j) = self.jit.as_deref_mut() {
									j.t2.note_break();
									j.t2.note_retire(ran);
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
						#[cfg(feature = "codegen")]
						if let Some(j) = self.jit.as_deref_mut() {
							j.record(h.tag, r);
						}
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
								// Recording mode reports potential coverage. AOT
								// counts only instructions actually run above;
								// a stale slot can fail verification and fall back.
								if !t.aot && compiled.is_some() {
									t.covered += r;
								}
								let w = !t.aot && (t.total >> 22) & 7 == 0;
								if w && !t.window {
									// fresh window: never chain an edge
									// across the unrecorded gap
									t.t2.note_break();
								}
								t.window = w;
								if w && compiled.is_none() && t.miss.len() < 1 << 20 {
									*t.miss.entry(h.tag).or_insert(0) += r;
								}
								match w {
									true => t.t2.note_block(h.tag, r),
									false => t.t2.note_retire(r),
								}
							}
						}
						done += r;
					} else if (self.pc & 0xfff) <= 0xff8 && self.build_block(slot) {
						// A freshly built block whose pc is a baked-region
						// entry installs its dispatch slot HERE — matching by
						// entry pc, not by re-forming the profiler's regions:
						// live formation is sampled and never reproduces the
						// same member sets (436 of 2681 entries matched), and
						// content verification already carries the safety.
						#[cfg(feature = "aot")]
						if self.tier2.as_ref().map_or(false, |t| t.aot) {
							let pc = self.block_heads[slot].tag;
							self.aot_try_install(slot, pc);
						}
						let r = self.exec_block(slot);
						#[cfg(feature = "codegen")]
						{
							let tag = self.block_heads[slot].tag;
							if let Some(j) = self.jit.as_deref_mut() {
								j.record(tag, r);
							}
						}
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
								if !t.aot && t.t2.lookup(tag, g).is_some() {
									t.covered += r;
								}
								let w = !t.aot && (t.total >> 22) & 7 == 0;
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
						#[cfg(feature = "codegen")]
						if let Some(j) = self.jit.as_deref_mut() {
							j.t2.note_break();
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
			self.retired = self.retired.wrapping_add(done - idle_charged);
			remaining = remaining.saturating_sub(done);
			// Device-service boundary, at the same stream position as the
			// old per-tick countdown: the end of the interval's last
			// instruction, delivery attempted in the same step. Blocks may
			// overshoot the boundary by up to BLOCK_MAX-1 instructions; the
			// device clocks advance by the true retired count either way,
			// so guest time stays tied to instructions retired.
			if self.since_service >= self.tick_interval {
				let served = self.since_service;
				self.since_service = 0;
				self.mmu.tick(served, &mut self.csr[CSR_MIP_ADDRESS as usize]);
				self.check_interrupt = false;
				self.handle_interrupt(self.pc);
				#[cfg(feature = "tier2")]
				if self.tier2.as_ref().map_or(false, |t| t.t2.due()) {
					self.tier2_form_pass();
				}
				#[cfg(feature = "codegen")]
				if self.jit.as_ref().map_or(false, |j| j.t2.due()) {
					self.jit_form_pass();
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
			code_gen: self.mmu.code_gen(),
			page_gen: self.mmu.page_gen(phys_page),
			glob_gen: self.mmu.glob_gen()
		};
	}

	/// risc-box patch (tier2): switch the dispatcher on, dumping every
	/// formed region to `dump` when given (the AOT bake pipeline's input).
	#[cfg(feature = "tier2")]
	pub fn tier2_enable(&mut self, dump: Option<&std::path::Path>) {
		let mut t2 = ::jit::Tier2::new(Box::new(::jit::RecordBackend::new(dump)));
		Self::tier2_tune(&mut t2);
		// the bake pipeline wants maximal discovery: a formation that fails
		// today (an evicted cache slot, an emit gap) may succeed next pass
		t2.blacklist_on_fail = false;
		self.tier2 = Some(Box::new(Tier2State {
			t2,
			lay: Self::tier2_layout(),
			covered: 0,
			total: 0,
			window: false,
			passes: 0,
			aot: false,
			miss: std::collections::HashMap::new(),
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
			aot: true,
			miss: std::collections::HashMap::new(),
		}));
		self.aot_slots = vec![AotSlot::EMPTY; BLOCK_SLOTS];
		self.aot_vstate = vec![AotVerify::NEVER; AOT_FNS.len()];
	}

	#[cfg(feature = "aot")]
	pub fn aot_baked(&self) -> usize {
		AOT_FNS.len()
	}

	/// Install the dispatch slot for a block at `pc` if any baked region
	/// lists it as an entry AND verifies against live memory. Candidates
	/// come biggest-first; a region mixing another process's pages simply
	/// fails verification here and the next (purer) candidate gets its
	/// turn.
	#[cfg(feature = "aot")]
	fn aot_try_install(&mut self, slot: usize, pc: u64) {
		let Ok(i) = AOT_ENTRY_PCS.binary_search(&pc) else { return };
		for &(handle, entry) in AOT_ENTRY_LISTS[i] {
			if self.aot_verified(handle) {
				self.aot_slots[slot] = AotSlot {
					tag: pc,
					handle,
					entry,
					gen: self.mmu.code_gen(),
				};
				self.aot_install_ok += 1;
				return;
			}
		}
		self.aot_install_fail += 1;
	}

	/// May baked region `handle` run right now?
	///
	/// Two-level cache, because verification cadence was the first AOT
	/// run's whole cost (42 of ~195 MIPS): a full content compare on every
	/// context switch is 200x the work of re-probing the mappings.
	/// - content (the dumped (word,len) streams really are in memory) is
	///   proven once per code_gen; the check exec-marks every member page,
	///   so any later write bumps code_gen and re-proves.
	/// - mapping (member vpcs still hit those phys pages) is re-probed once
	///   per tlb_gen, including kernel mappings which a guest may replace.
	#[cfg(feature = "aot")]
	#[inline(always)]
	fn aot_verified(&mut self, handle: u32) -> bool {
		let tg = self.mmu.tlb_gen();
		let cg = self.mmu.code_gen();
		let v = &self.aot_vstate[handle as usize];
		if v.content_gen == cg && v.tlb_gen == tg {
			return v.ok;
		}
		self.aot_verify_slow(handle, tg, cg)
	}

	#[cfg(feature = "aot")]
	fn aot_verify_slow(&mut self, handle: u32, tg: u32, cg: u32) -> bool {
		let v = &self.aot_vstate[handle as usize];
		// Mapping equality only preserves a SUCCESSFUL content proof. A
		// failed check can still have recorded every physical page, so it
		// must never become valid merely because those pages did not move.
		// A changed mapping needs a fresh instruction check too: it may now
		// point to either matching code or a different program.
		// risc-box patch (per-page generations): the epoch moving does not void a SUCCESSFUL proof whose pages were
		// none of them written since (and memory was not wholesale invalidated).
		if v.ok && v.content_gen != 0 && v.content_gen != cg && v.glob == self.mmu.glob_gen()
			&& v.pgens.len() == v.phys.len()
			&& v.phys.iter().zip(v.pgens.iter()).all(|(&pp, &g)| self.mmu.page_gen(pp) == g) {
			self.aot_vstate[handle as usize].content_gen = cg;
		}
		let v = &self.aot_vstate[handle as usize];
		if v.content_gen == cg && v.ok && self.aot_verify_mapping(handle) {
			self.aot_vstate[handle as usize].tlb_gen = tg;
			return true;
		}
		self.aot_verify_content(handle)
	}

	/// Full content check: every member's pc translates and the code there
	/// decodes to exactly the dumped (word, len) stream — the same
	/// uncompress build_block applies, so "verified" means the baked ops
	/// are what the interpreter would decode from this memory. Marks every
	/// member page executable so the SMC snoop guards it from now on, and
	/// records the phys pages for the cheap per-tlb_gen mapping re-probe.
	#[cfg(feature = "aot")]
	fn aot_verify_content(&mut self, handle: u32) -> bool {
		let cg = self.mmu.code_gen();
		let members = AOT_MEMBERS[handle as usize];
		let mut phys = Vec::with_capacity(members.len());
		let mut ok = true;
		'members: for &(start, words) in members {
			let p_start = match self.mmu.translate_fetch_probe(start) {
				Ok(p) => p,
				Err(_) => {
					ok = false;
					break 'members;
				}
			};
			phys.push(p_start & !0xfff);
			if !self.mmu.mark_exec_page(p_start) {
				ok = false;
				break 'members;
			}
			let mut off = start & 0xfff;
			for &(word, len) in words {
				let p = (p_start & !0xfff) | off;
				let raw = self.mmu.load_word_raw(p);
				let (w, l) = match (raw & 0x3) == 0x3 {
					true => (raw, 4u8),
					false => (self.uncompress(raw & 0xffff), 2u8),
				};
				if w != word || l != len {
					ok = false;
					break 'members;
				}
				off += len as u64;
			}
		}
		let tg = self.mmu.tlb_gen();
		self.aot_vstate[handle as usize] =
			{
				let pgens: Vec<u32> = phys.iter().map(|&pp| self.mmu.page_gen(pp)).collect();
				let glob = self.mmu.glob_gen();
				AotVerify { content_gen: cg, tlb_gen: tg, phys, pgens, glob, ok }
			};
		ok
	}

	/// Mapping re-probe: the content is already proven for this code_gen;
	/// just confirm each member vpc still translates to the phys page it
	/// was proven on. One TLB probe per member block.
	#[cfg(feature = "aot")]
	fn aot_verify_mapping(&mut self, handle: u32) -> bool {
		let members = AOT_MEMBERS[handle as usize];
		let expect = &self.aot_vstate[handle as usize].phys;
		if expect.len() != members.len() {
			return false;
		}
		for (i, &(start, _)) in members.iter().enumerate() {
			match self.mmu.translate_fetch_probe(start) {
				Ok(p) if (p & !0xfff) == expect[i] => {}
				_ => return false,
			}
		}
		true
	}

	/// The Layout coverage/bake runs hand emit_region. Only DETERMINISM
	/// matters there (the AOT keys on hash_blocks, the module is advisory),
	/// so the values just have to be fixed and plausible. The live JIT builds
	/// the machine's real layout in jit_layout.
	#[cfg(feature = "tier2")]
	fn tier2_layout() -> ::jit::Layout {
		::jit::Layout {
			memory64: false,
			shared: false,
			max_pages: None,
			ctx: 2048,
			x_base: 0,
			f_base: 512,
			pc_addr: 256,
			gen_addr: 264,
			baked_gen: 0,
			fcsr_addr: 272,
			res_flag_addr: 280,
			res_addr_addr: 288,
			tlb: None,
			guest_dram_base: 0x8000_0000,
			dram_len: 1 << 31,
			ram: ::jit::Ram::Flat { dram_base: 4096 },
		}
	}

	/// risc-box patch (codegen): switch the live JIT on. False when the
	/// platform verb is absent or its budget already spent: the machine
	/// interprets exactly as before.
	#[cfg(feature = "codegen")]
	pub fn jit_enable(&mut self, params: JitParams) -> bool {
		let policy = ::jit::verb::Policy {
			max_module_bytes: params.max_module_bytes,
			..Default::default()
		};
		if !::jit::verb::enable(policy) {
			return false;
		}
		let mut t2 = ::jit::Tier2::new(Box::new(::jit::RecordBackend::new(None)));
		t2.greedy = true;
		t2.max_blocks = params.max_blocks;
		t2.min_heat = params.seed_heat;
		t2.form_interval = params.form_interval;
		let lay = self.jit_layout(params.max_pages);
		let lay_hash = ::jit::layout_hash(&lay);
		self.jit = Some(Box::new(JitState {
			t2,
			lay,
			lay_hash,
			slots: vec![JitSlot::EMPTY; BLOCK_SLOTS],
			present: vec![0; BLOCK_SLOTS / 64],
			regions: Vec::new(),
			free: Vec::new(),
			instances: Default::default(),
			volatile: Default::default(),
			owner: ::jit::verb::thread_id(),
			live: false,
			since: std::time::Instant::now(),
			total: 0,
			window: false,
			params,
			stats: JitStats::default(),
			diag: JitDiag::default(),
		}));
		true
	}

	/// risc-box patch (codegen): this machine's JIT counters, None when off.
	#[cfg(feature = "codegen")]
	pub fn jit_stats(&self) -> Option<JitStats> {
		self.jit.as_ref().map(|j| j.stats.clone())
	}

	/// risc-box patch (diag): hottest regions and sampled exit classes/pcs, as JSON (None when the JIT is off).
	#[cfg(feature = "codegen")]
	pub fn jit_diag(&self) -> Option<String> {
		self.jit.as_ref().map(|j| j.diag_json())
	}

	/// The machine's real layout, as offsets from the Cpu itself (the
	/// context block carries the Cpu's address, so a moved Cpu needs no new
	/// code) plus the RAM geometry.
	#[cfg(feature = "codegen")]
	fn jit_layout(&self, max_pages: u64) -> ::jit::Layout {
		let base = self as *const Cpu as usize as u64;
		let (tlb, sets) = self.mmu.jit_tlb();
		let (_, _, _, len) = self.mmu.jit_ram();
		let off = |a: u64| a - base;
		::jit::Layout {
			memory64: cfg!(target_pointer_width = "64"),
			shared: cfg!(target_feature = "atomics"),
			max_pages: Some(max_pages),
			ctx: ::jit::verb::CTX.addr(),
			x_base: off(self.x.as_ptr() as usize as u64),
			f_base: off(self.f.as_ptr() as usize as u64),
			pc_addr: off(&self.pc as *const u64 as usize as u64),
			gen_addr: 0,
			baked_gen: 0,
			fcsr_addr: off(&self.csr[CSR_FCSR_ADDRESS as usize] as *const u64 as usize as u64),
			res_flag_addr: off(&self.is_reservation_set as *const bool as usize as u64),
			res_addr_addr: off(&self.reservation as *const u64 as usize as u64),
			tlb: Some(::jit::TlbLayout {
				sets,
				read_tags: off(tlb[0]),
				read_metas: off(tlb[1]),
				read_ppns: off(tlb[2]),
				write_tags: off(tlb[3]),
				write_metas: off(tlb[4]),
				write_ppns: off(tlb[5]),
				meta_cache: off(tlb[6]),
			}),
			guest_dram_base: ::mmu::DRAM_BASE,
			dram_len: len,
			ram: ::jit::Ram::Chunked {
				store_bail: vec![(
					::mmu::FB_STORE_WINDOW.0 - ::mmu::DRAM_BASE,
					::mmu::FB_STORE_WINDOW.1 - ::mmu::DRAM_BASE,
				)],
			},
		}
	}

	/// Per run(): may this call dispatch, and point the context block at
	/// this machine's RAM tables (they move only when RAM is re-initialized;
	/// a new RAM size invalidates every installed region).
	#[cfg(feature = "codegen")]
	fn jit_prepare(&mut self) {
		let (rd, wr, marks, len) = self.mmu.jit_ram();
		let rv64 = matches!(self.xlen, Xlen::Bit64);
		let j = match self.jit.as_deref_mut() {
			Some(j) => j,
			None => return,
		};
		j.live = rv64 && ::jit::verb::thread_id() == j.owner;
		if !j.live {
			return;
		}
		if len != j.lay.dram_len {
			j.clear();
			j.lay.dram_len = len;
			j.lay_hash = ::jit::layout_hash(&j.lay);
		}
		let ctx = &::jit::verb::CTX;
		ctx.set(::jit::CTX_RD, rd);
		ctx.set(::jit::CTX_WR, wr);
		ctx.set(::jit::CTX_MARKS, marks);
	}

	/// Run the compiled region entered at block `slot` (tag already matched
	/// by the block probe), if one is installed and proven for the current
	/// generation and translation. Returns instructions retired; 0 = not
	/// run (the caller interprets the block).
	#[cfg(feature = "codegen")]
	#[inline(always)]
	fn jit_run(&mut self, slot: usize, tag: u64) -> u64 {
		// risc-box patch (chaining): a region exits to the dispatcher at any pc outside it; when that pc starts
		// another region proven for the current (generation, translation), run it straight away, inside the
		// same fuel, instead of paying the block-cache probe and run-loop bookkeeping between them. Stops on a
		// pending interrupt check, an empty call, or spent fuel, exactly where a lone call would have returned.
		let (fuel, chain) = match self.jit.as_deref() {
			Some(j) if j.live => (j.params.fuel, j.params.chain),
			_ => return 0,
		};
		let (mut slot, mut tag) = (slot, tag);
		let mut total = 0u64;
		loop {
			let ran = self.jit_run_one(slot, tag, fuel - total);
			total += ran;
			if ran == 0 || !chain || self.check_interrupt || total + 16 > fuel {
				return total;
			}
			tag = self.pc;
			slot = ((tag >> 1) as usize) & (BLOCK_SLOTS - 1);
			if let Some(j) = self.jit.as_deref_mut() {
				j.stats.chained += 1;
			}
		}
	}

	#[cfg(feature = "codegen")]
	#[inline(always)]
	fn jit_run_one(&mut self, slot: usize, tag: u64, fuel: u64) -> u64 {
		let (rid, entry, fresh, ok) = match self.jit.as_deref() {
			Some(j) if j.live => {
				if (j.present[slot >> 6] >> (slot & 63)) & 1 == 0 {
					return 0;
				}
				let s = j.slots[slot];
				if s.tag != tag {
					return 0;
				}
				let r = &j.regions[s.region as usize];
				let now = (self.mmu.code_gen(), self.mmu.tlb_meta_value());
				(s.region, s.entry, r.checked == now, r.ok)
			}
			_ => return 0,
		};
		let ok = match fresh {
			true => ok,
			false => self.jit_verify(rid),
		};
		if !ok {
			return 0;
		}
		let (index, bias) = {
			let j = self.jit.as_deref().unwrap();
			let r = &j.regions[rid as usize];
			(r.index, r.bias)
		};
		// The generated code reaches this Cpu through the context block: its
		// address is taken here, next to the call that uses it.
		let ctx = &::jit::verb::CTX;
		ctx.set(::jit::CTX_BASE, self as *mut Cpu as usize as u64);
		ctx.set(::jit::CTX_BIAS, bias);
		let ran = unsafe { ::jit::verb::call(index, fuel, entry) };
		let pc = self.pc;
		let j = self.jit.as_deref_mut().unwrap();
		j.stats.calls += 1;
		j.stats.retired += ran;
		if ran == 0 {
			j.stats.empty_calls += 1;
		}
		{
			let r = &mut j.regions[rid as usize];
			r.calls += 1;
			r.retired += ran;
			r.empty += (ran == 0) as u64;
		}
		if j.stats.calls & 63 == 0 {
			j.diag_exit(rid, pc);
		}
		ran
	}

	/// Prove region `rid` for the current generation and translation (the
	/// AOT verifier's two levels; see JitState). False: do not run it now.
	///
	/// risc-box patch (per-page generations, two address spaces): a content proof stands while every page it
	/// read keeps its page generation, so stores elsewhere (a guest JIT writing its code) cost a few compares,
	/// not a re-read; a second proof kept per region means a process switch between two address spaces with
	/// the same code re-probes mappings only; and a failed proof here leaves the other proofs alone. A pc turns
	/// volatile only when code changed on a physical page this region was proven on, never because another
	/// process maps different code at the same virtual address.
	#[cfg(feature = "codegen")]
	fn jit_verify(&mut self, rid: u32) -> bool {
		let mut j = match self.jit.take() {
			Some(j) => j,
			None => return false,
		};
		let cg = self.mmu.code_gen();
		let gg = self.mmu.glob_gen();
		let ok = {
			let r = &mut j.regions[rid as usize];
			let mut ok = false;
			// level 1: the primary proof's pages unwritten and still mapped here
			if r.proof_gen != 0 && !r.pages.is_empty()
				&& (r.proof_gen == cg || self.jit_pages_unwritten(&r.pages, &r.page_gens, r.proof_glob, gg)) {
				r.proof_gen = cg;
				j.stats.map_checks += 1;
				ok = self.jit_pages_mapped(&r.pages);
			}
			// level 1b: the other address space's proof
			if !ok {
				let alt_ok = match &r.alt {
					Some((pages, gens, glob)) => {
						self.jit_pages_unwritten(pages, gens, *glob, gg) && self.jit_pages_mapped(pages)
					}
					None => false,
				};
				if alt_ok {
					let (pages, gens, glob) = r.alt.take().unwrap();
					if r.proof_gen != 0 && !r.pages.is_empty() {
						r.alt = Some((std::mem::take(&mut r.pages), std::mem::take(&mut r.page_gens), r.proof_glob));
					}
					r.pages = pages;
					r.page_gens = gens;
					r.proof_glob = glob;
					r.proof_gen = cg;
					j.stats.map_checks += 1;
					ok = true;
				}
			}
			if !ok {
				// level 2, full proof against the CURRENT mapping: every member translates, its page is marked
				// executable (so a later store moves that page's generation), and the code there is the
				// (word, len) stream the module was built from — the same uncompress build_block applies.
				j.stats.content_checks += 1;
				ok = true;
				let mut np: Vec<(u64, u64)> = Vec::new();
				let mut changed: Option<(u64, u64)> = None; // (member start, physical page) that differed
				'members: for (start, words) in r.members.iter() {
					let vpage = start & !0xfff;
					let p = match np.iter().find(|pg| pg.0 == vpage) {
						Some(&(_, page)) => page | (start & 0xfff),
						None => {
							let p = match self.mmu.translate_fetch_probe(vpage) {
								Ok(p) => p,
								Err(_) => {
									ok = false;
									break;
								}
							};
							if !self.mmu.mark_exec_page(p) {
								ok = false;
								break;
							}
							np.push((vpage, p & !0xfff));
							(p & !0xfff) | (start & 0xfff)
						}
					};
					let mut off = start & 0xfff;
					for &(word, len) in words.iter() {
						let raw = self.mmu.load_word_raw((p & !0xfff) | off);
						let (w, l) = match (raw & 0x3) == 0x3 {
							true => (raw, 4u8),
							false => (self.uncompress(raw & 0xffff), 2u8),
						};
						if w != word || l != len {
							ok = false;
							changed = Some((*start, p & !0xfff));
							break 'members;
						}
						off += len as u64;
					}
				}
				// A page-table walk above may have stored an A bit into a marked page (moving the epoch and, if it
				// hit one of OUR pages, that page's generation): such a proof must not stand.
				if self.mmu.code_gen() != cg {
					ok = false;
				}
				if ok {
					let ng: Vec<u32> = np.iter().map(|&(_, pp)| self.mmu.page_gen(pp)).collect();
					if r.proof_gen != 0 && !r.pages.is_empty() && r.pages != np {
						r.alt = Some((std::mem::take(&mut r.pages), std::mem::take(&mut r.page_gens), r.proof_glob));
					}
					r.pages = np;
					r.page_gens = ng;
					r.proof_glob = gg;
					r.proof_gen = self.mmu.code_gen();
				} else if let Some((pc, ppage)) = changed {
					// Different code on a page we were proven on = the code really changed: those proofs are dead
					// and the pc is volatile. Different code on some other physical page = another address space
					// that does not hold this code: leave the proofs and the pc alone.
					let ours = r.pages.iter().any(|&(_, pp)| pp == ppage)
						|| r.alt.as_ref().map_or(false, |(pages, _, _)| pages.iter().any(|&(_, pp)| pp == ppage));
					if ours {
						r.pages.clear();
						r.page_gens.clear();
						r.proof_gen = 0;
						if r.alt.as_ref().map_or(false, |(pages, _, _)| pages.iter().any(|&(_, pp)| pp == ppage)) {
							r.alt = None;
						}
						if j.volatile.len() >= 1 << 16 {
							j.volatile.clear();
						}
						if j.volatile.insert(pc) {
							j.stats.volatile += 1;
						}
					}
				}
			}
			r.ok = ok;
			r.checked = (self.mmu.code_gen(), self.mmu.tlb_meta_value());
			if !ok {
				j.stats.verify_failures += 1;
			}
			ok
		};
		self.jit = Some(j);
		ok
	}

	/// Every page of a proof still carries the generation it was proven at (and memory was not wholesale
	/// invalidated since): no store reached any of them.
	#[cfg(feature = "codegen")]
	#[inline(always)]
	fn jit_pages_unwritten(&self, pages: &[(u64, u64)], gens: &[u32], glob: u32, gg: u32) -> bool {
		glob == gg && pages.len() == gens.len()
			&& pages.iter().zip(gens.iter()).all(|(&(_, pp), &g)| self.mmu.page_gen(pp) == g)
	}

	/// Every (virtual, physical) page of a proof still translates the same way here.
	#[cfg(feature = "codegen")]
	fn jit_pages_mapped(&mut self, pages: &[(u64, u64)]) -> bool {
		for &(vpage, page) in pages.iter() {
			match self.mmu.translate_fetch_probe(vpage) {
				Ok(p) if (p & !0xfff) == page => {}
				_ => return false,
			}
		}
		true
	}

	/// A cached block's ops, if the cache holds `pc` for this generation.
	#[cfg(feature = "codegen")]
	fn jit_block_ops(&self, pc: u64, cg: u32) -> Option<Vec<BlockOp>> {
		let slot = ((pc >> 1) as usize) & (BLOCK_SLOTS - 1);
		let h = self.block_heads[slot];
		if h.tag != pc || h.count == 0 || (h.code_gen != cg && !self.head_valid(&h)) {
			return None;
		}
		let base = slot * BLOCK_MAX;
		Some(self.block_ops[base..base + h.count as usize].to_vec())
	}

	/// A lone block is worth a region only when it loops on itself.
	#[cfg(feature = "codegen")]
	fn jit_self_loop(start: u64, ops: &[BlockOp]) -> bool {
		let mut pc = start;
		for o in ops {
			let target = pc.wrapping_add(o.imm as i64 as u64);
			match o.kind {
				HOT_BEQ..=HOT_BGEU | HOT_JAL if target == start => return true,
				_ => {}
			}
			pc = pc.wrapping_add(o.len as u64);
		}
		false
	}

	/// Formation pass: form regions from sampled heat (blocks already
	/// covered by an installed region excluded, so compiles never overlap),
	/// emit each against the real layout at a page-aligned bias, and install
	/// it — from the verb's cache when these exact bytes were compiled
	/// before (any machine, any address), else compiled if the budget
	/// policy admits it (at most max_compiles_per_pass per pass).
	#[cfg(feature = "codegen")]
	fn jit_form_pass(&mut self) {
		let mut j = match self.jit.take() {
			Some(j) => j,
			None => return,
		};
		j.stats.passes += 1;
		if !j.live {
			j.t2.reset_form_clock();
			self.jit = Some(j);
			return;
		}
		// covered: installed AND runnable at its last check. A region that
		// cannot run here (another address space's code at the same pcs)
		// must not hide this context's hot blocks from formation.
		let regions = {
			let JitState { ref mut t2, ref slots, ref regions, ref volatile, .. } = *j;
			t2.form_now(|pc| {
				let s = slots[((pc >> 1) as usize) & (BLOCK_SLOTS - 1)];
				(s.tag == pc && regions[s.region as usize].ok) || volatile.contains(&pc)
			})
		};
		let cg = self.mmu.code_gen();
		let began = std::time::Instant::now();
		let installs_before = j.stats.installs;
		let mut waiting: Vec<PackCand> = Vec::new();
		let mut considered = 0u64;
		let mut installed = 0u64;
		for (members, _) in regions {
			j.stats.formed += 1;
			// One address space, one privilege side: members must be cached
			// for this generation, fetch from the page their block was built
			// from under the CURRENT translation, and sit on the hottest
			// member's side of the address space (sampled edges cross traps
			// and context switches; a region mixing them could never verify).
			let side = members.iter().max_by_key(|m| m.1).map_or(0, |m| m.0 >> 63);
			let mut blocks: Vec<(u64, u64, Vec<BlockOp>)> = Vec::with_capacity(members.len());
			for &(pc, h) in members.iter() {
				if pc >> 63 != side {
					continue;
				}
				let slot = ((pc >> 1) as usize) & (BLOCK_SLOTS - 1);
				let page = self.block_heads[slot].phys_page;
				match self.mmu.translate_fetch_probe(pc) {
					Ok(p) if (p & !0xfff) == page => {}
					_ => continue,
				}
				// Code on a page that keeps being rewritten while it holds code (a guest JIT's code pool) would
				// spend the process-lifetime module budget on code that is gone again soon: leave it to the
				// interpreter. A recycled page is rewritten once or twice; a JIT pool far more.
				if self.mmu.page_gen(page) >= JIT_REWRITTEN_PAGE_GEN {
					j.stats.skipped_rewritten += 1;
					continue;
				}
				if let Some(ops) = self.jit_block_ops(pc, cg) {
					blocks.push((pc, h, ops));
				}
			}
			if blocks.is_empty() || (blocks.len() == 1 && !Self::jit_self_loop(blocks[0].0, &blocks[0].2)) {
				continue;
			}
			// a block whose first op the emitter cannot translate is never an
			// entry worth having: every call there would return at once; a
			// near-cold member costs compile time and buys nothing
			let prune = j.params.prune_heat;
			blocks.retain(|b| ::jit::translatable(&b.2[0]) && b.1 >= prune);
			if blocks.is_empty() || (blocks.len() == 1 && !Self::jit_self_loop(blocks[0].0, &blocks[0].2)) {
				continue;
			}
			considered += 1;
			// risc-box patch (packed modules): a region compiled before (into whichever module, by whichever
			// machine) is installed from its placement without a compile; the rest wait to be packed
			let c = PackCand::new(blocks, &j.params, j.lay_hash);
			match ::jit::verb::placed(c.key) {
				Some(Some((index, base, n))) if n as usize == c.blocks.len() => {
					j.place(&c.blocks, index, c.bias, base);
					j.stats.pack_reused += 1;
					installed += 1;
				}
				Some(_) => {}
				None => waiting.push(c),
			}
		}
		installed += j.compile_packs(waiting);
		j.stats.refused += considered.saturating_sub(installed);
		let us = began.elapsed().as_micros() as u64;
		j.stats.form_us += us;
		if j.params.trace {
			let v = ::jit::verb::stats();
			eprintln!(
				"[jit] pass {} retired {}M: +{} installs, {} compiled ({} failed, {:.0} ms total), {} live, coverage {:.1}%, pass {:.1} ms",
				j.stats.passes, self.retired / 1_000_000, j.stats.installs - installs_before, v.compiled, v.failed,
				v.compile_us as f64 / 1000.0, j.stats.live_regions,
				100.0 * j.stats.retired as f64 / (j.stats.retired + j.stats.interpreted).max(1) as f64,
				us as f64 / 1000.0
			);
		}
		self.jit = Some(j);
	}

	/// The heaviest UNCOVERED pcs (sampled), heaviest first.
	#[cfg(feature = "tier2")]
	pub fn tier2_miss_top(&self, n: usize) -> Vec<(u64, u64)> {
		match self.tier2.as_ref() {
			Some(t) => {
				let mut v: Vec<(u64, u64)> =
					t.miss.iter().map(|(&pc, &h)| (pc, h)).collect();
				v.sort_by(|a, b| b.1.cmp(&a.1));
				v.truncate(n);
				v
			}
			None => Vec::new(),
		}
	}

	#[cfg(feature = "aot")]
	pub fn aot_install_stats(&self) -> (u64, u64) {
		(self.aot_install_ok, self.aot_install_fail)
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
			let Tier2State { ref mut t2, ref lay, aot, .. } = *t;
			let _ = aot;
			#[cfg(feature = "aot")]
			if aot {
				t2.prune_stale(gen);
			}
			// aot mode never records heat/edges, so forming would be a no-op
			// walk over empty maps; note_retire still advances the clock that
			// paces the heal sweep. Coverage/profiling mode forms as before.
			#[cfg(feature = "aot")]
			let skip_form = aot;
			#[cfg(not(feature = "aot"))]
			let skip_form = false;
			match skip_form {
				true => t2.reset_form_clock(),
				false => t2.maybe_form(lay, gen, |pc| self.tier2_ops_of(pc)),
			}
		}
		#[cfg(feature = "aot")]
		if t.aot {
			// Heal sweep: a block built while its region's members were not
			// yet paged in failed verification once and would otherwise stay
			// uninstalled until eviction. Walk the live block cache and
			// (re)install every baked entry that verifies NOW. Also covers
			// entries whose slot was clobbered by an aliasing install.
			let gen = self.mmu.code_gen();
			for slot in 0..BLOCK_SLOTS {
				let h = self.block_heads[slot];
				if h.tag == 0 || (h.code_gen != gen && !self.head_valid(&h)) {
					continue;
				}
				if self.aot_slots[slot].tag == h.tag && self.aot_slots[slot].gen == gen {
					continue;
				}
				self.aot_try_install(slot, h.tag);
			}
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
			if op.kind <= HOT_STORE_MAX && self.mmu.code_gen() != head.code_gen
				&& !(self.mmu.glob_gen() == head.glob_gen && self.mmu.page_gen(head.phys_page) == head.page_gen) {
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
		self.block_builds += 1;
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
			code_gen: self.mmu.code_gen(),
			page_gen: self.mmu.page_gen(p_start & !0xfff),
			glob_gen: self.mmu.glob_gen()
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
		self.exec_op_impl(e.kind, e, address)
	}

	/// risc-box patch (aot): the same dispatch with the kind a compile-time
	/// constant. Each monomorphization folds the match below to one arm, so
	/// baked regions execute exactly the interpreter's op bodies — one
	/// source of truth — without the runtime kind dispatch.
	#[cfg(feature = "aot")]
	#[inline(always)]
	pub(crate) fn exec_op_const<const K: u8>(&mut self, e: &BlockOp, address: u64) -> Result<(), Trap> {
		self.exec_op_impl(K, e, address)
	}

	#[inline(always)]
	fn exec_op_impl(&mut self, kind: u8, e: &BlockOp, address: u64) -> Result<(), Trap> {
		let rd = e.rd as usize;
		let rs1 = e.rs1 as usize;
		let rs2 = e.rs2 as usize;
		let imm = e.imm as i64;
		match kind {
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
				// risc-box patch (fp spec): NaN-boxed, as the table's FLW
				self.f[rd] = match self.mmu.load_word(self.x[rs1].wrapping_add(imm) as u64) {
					Ok(data) => f64::from_bits(FP_BOX | data as u64),
					Err(e) => return Err(e)
				};
			},
			// risc-box patch (fp spec): the table entries' helpers (canonical
			// NaN results, NV, FDIV's IEEE zero-divisor rules)
			HOT_FADD_D => {
				let (a, b) = (self.f[rs1], self.f[rs2]);
				self.f[rd] = self.fp_res_d(a + b, &[a, b]);
			},
			HOT_FSUB_D => {
				let (a, b) = (self.f[rs1], self.f[rs2]);
				self.f[rd] = self.fp_res_d(a - b, &[a, b]);
			},
			HOT_FMUL_D => {
				let (a, b) = (self.f[rs1], self.f[rs2]);
				self.f[rd] = self.fp_res_d(a * b, &[a, b]);
			},
			HOT_FDIV_D => {
				let (a, b) = (self.f[rs1], self.f[rs2]);
				self.f[rd] = self.fp_div_d(a, b);
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

	fn set_fcsr_nv(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= FFLAG_NV;
	}

	fn set_fcsr_dz(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= FFLAG_DZ;
	}

	fn _set_fcsr_of(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x4;
	}

	fn _set_fcsr_uf(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= 0x2;
	}

	fn set_fcsr_nx(&mut self) {
		self.csr[CSR_FCSR_ADDRESS as usize] |= FFLAG_NX;
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

	/// risc-box patch (snapshot): the architectural state, then the bus.
	/// Caches (decode, block, TLB, AOT slots) are not written: they are
	/// rebuilt on demand and a fresh Cpu starts with them empty anyway.
	pub fn snapshot_into(&self, s: &mut ::snapshot::Ser, level: u8) -> ::snapshot::SnapshotStats {
		self.snapshot_cpu(s);
		self.mmu.snapshot_into(s, level, true)
	}

	/// risc-box patch (fork): every section except RAM_ (the RAM is shared
	/// as an image instead of serialized).
	pub fn snapshot_into_no_ram(&self, s: &mut ::snapshot::Ser, level: u8) {
		self.snapshot_cpu(s);
		self.mmu.snapshot_into(s, level, false);
	}

	fn snapshot_cpu(&self, s: &mut ::snapshot::Ser) {
		let at = s.begin_section(b"CPU_");
		s.u64(self.clock);
		s.u8(match self.xlen { Xlen::Bit32 => 32, Xlen::Bit64 => 64 });
		s.u8(::mmu::privilege_code(&self.privilege_mode));
		s.bool(self.wfi);
		for v in self.x.iter() {
			s.i64(*v);
		}
		for v in self.f.iter() {
			s.f64(*v);
		}
		s.u64(self.pc);
		s.u32(CSR_CAPACITY as u32);
		for v in self.csr.iter() {
			s.u64(*v);
		}
		s.u64(self.reservation);
		s.bool(self.is_reservation_set);
		s.u64(self.since_service);
		s.bool(self.check_interrupt);
		s.end_section(at);
	}

	/// risc-box patch (snapshot): Ok(false) = not a section this layer knows.
	pub fn restore_section(&mut self, tag: &[u8; 4], payload: &[u8], stats: &mut ::snapshot::RestoreStats) -> Result<bool, String> {
		if tag != b"CPU_" {
			return self.mmu.restore_section(tag, payload, stats);
		}
		let mut r = ::snapshot::De::new(payload);
		self.clock = r.u64()?;
		let xlen = match r.u8()? {
			32 => Xlen::Bit32,
			64 => Xlen::Bit64,
			v => return Err(format!("snapshot: bad xlen {}", v))
		};
		self.privilege_mode = ::mmu::privilege_from(r.u8()?)?;
		self.wfi = r.bool()?;
		for i in 0..32 {
			self.x[i] = r.i64()?;
		}
		for i in 0..32 {
			self.f[i] = r.f64()?;
		}
		self.pc = r.u64()?;
		let n = r.u32()? as usize;
		if n != CSR_CAPACITY {
			return Err(format!("snapshot: {} CSRs, expected {}", n, CSR_CAPACITY));
		}
		for i in 0..CSR_CAPACITY {
			self.csr[i] = r.u64()?;
		}
		self.reservation = r.u64()?;
		self.is_reservation_set = r.bool()?;
		self.since_service = r.u64()?;
		self.check_interrupt = r.bool()?;
		r.finish("CPU_")?;
		// derived state: mask + the MMU's copies of what translation depends on
		self.update_xlen(xlen);
		self.mmu.update_privilege_mode(self.privilege_mode.clone());
		self.mmu.update_mstatus(self.read_csr_raw(CSR_MSTATUS_ADDRESS));
		self.decode_cache = DecodeCache::new();
		for h in self.block_heads.iter_mut() {
			*h = BlockHead::EMPTY;
		}
		Ok(true)
	}

	/// Returns mutable `Mmu`
	pub fn get_mut_mmu(&mut self) -> &mut Mmu {
		&mut self.mmu
	}

	/// Returns `Mmu` (risc-box patch: the immutable side of the pair — the
	/// host's framebuffer scanout reads DRAM without touching CPU state)
	/// risc-box patch: blocks decoded into the block cache since boot.
	pub fn block_builds(&self) -> u64 {
		self.block_builds
	}

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

// ===== risc-box patch: RISC-V floating-point semantics =====
// Upstream computed every float op with host Rust arithmetic and stored
// whatever the host produced. RISC-V differs from that in ways the guest's
// JS engines (SpiderMonkey, V8) depend on:
// - an arithmetic op whose result is a NaN produces the CANONICAL NaN
//   (0x7ff8000000000000 / 0x7fc00000). Host and wasm NaNs carry other
//   signs and payloads (x86 makes 0xfff8...), and NaN-boxing engines read
//   a stray payload as a tagged value.
// - a single lives NaN-BOXED in the 64-bit register (upper 32 bits all
//   ones), and a single input that is not properly boxed reads as the
//   canonical NaN. Loads and moves (FLW, FMV.W.X) box; stores and moves out
//   (FSW, FMV.X.W) take the low 32 bits raw.
// - float->int conversions round per the instruction's rm (Math.floor/
//   ceil/round compile to fcvt with RDN/RUP/RMM), saturate (NaN -> the
//   type's maximum), and report NV (NaN or out of range) and NX (inexact):
//   V8's and SpiderMonkey's RISC-V backends read exactly those flags to
//   decide whether a double held an int32.
// Flags kept: NV (signaling-NaN inputs, invalid operations, invalid
// conversions, comparisons), DZ (finite nonzero / zero), NX for float->int
// conversions only. OF/UF and arithmetic NX are not tracked; arithmetic and
// int->float conversions ignore rm and round to nearest-even (no JS engine
// changes frm). jit.rs reproduces every rule here bit for bit or leaves the
// op to the interpreter; mod test_jit_equivalence holds the two together.
pub(crate) const FP_CANON_D: u64 = 0x7ff8_0000_0000_0000;
pub(crate) const FP_CANON_S: u32 = 0x7fc0_0000;
pub(crate) const FP_BOX: u64 = 0xffff_ffff_0000_0000;
pub(crate) const FFLAG_NV: u64 = 0x10;
pub(crate) const FFLAG_DZ: u64 = 0x8;
pub(crate) const FFLAG_NX: u64 = 0x1;

fn snan_d(v: f64) -> bool {
	v.is_nan() && v.to_bits() & 0x0008_0000_0000_0000 == 0
}

fn snan_s(v: f32) -> bool {
	v.is_nan() && v.to_bits() & 0x0040_0000 == 0
}

/// The single a register holds: its low 32 bits when NaN-boxed, else the
/// canonical NaN.
fn s_unbox(reg: f64) -> f32 {
	let bits = reg.to_bits();
	match bits >= FP_BOX {
		true => f32::from_bits(bits as u32),
		false => f32::from_bits(FP_CANON_S)
	}
}

/// A single as the register value: NaN-boxed.
fn s_box(v: f32) -> f64 {
	f64::from_bits(FP_BOX | v.to_bits() as u64)
}

/// The integer type of a float->int conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FpInt {
	W,
	Wu,
	L,
	Lu
}

impl FpInt {
	/// A rounded value r converts without saturating iff lo <= r < hi (both
	/// bounds exact doubles; false for NaN).
	pub(crate) fn bounds(self) -> (f64, f64) {
		match self {
			FpInt::W => (-2147483648.0, 2147483648.0),
			FpInt::Wu => (0.0, 4294967296.0),
			FpInt::L => (-9223372036854775808.0, 9223372036854775808.0),
			FpInt::Lu => (0.0, 18446744073709551616.0)
		}
	}
}

/// Round to an integral value per a RISC-V rounding mode (0 RNE, 1 RTZ,
/// 2 RDN, 3 RUP, 4 RMM).
pub(crate) fn fp_round(v: f64, rm: u8) -> f64 {
	match rm {
		0 => v.round_ties_even(),
		1 => v.trunc(),
		2 => v.floor(),
		3 => v.ceil(),
		_ => v.round() // RMM: ties away from zero
	}
}

impl Cpu {
	/// The rounding mode a conversion uses: the word's rm field, DYN (7)
	/// reading frm. None for the reserved encodings (5, 6, or a frm of 5-7),
	/// which make the instruction illegal.
	fn fp_rm(&self, word: u32) -> Option<u8> {
		let rm = match (word >> 12) & 7 {
			7 => (self.csr[CSR_FCSR_ADDRESS as usize] >> 5) & 7,
			rm => rm as u64
		};
		match rm <= 4 {
			true => Some(rm as u8),
			false => None
		}
	}

	/// FCVT.{W,WU,L,LU}.{S,D}: round `v` per rm, then saturate (NaN and
	/// positive overflow -> the maximum, negative overflow -> the minimum,
	/// for unsigned 0) raising NV, or convert exactly raising NX if rounding
	/// changed the value. 32-bit results are sign-extended.
	fn fp_to_int(&mut self, v: f64, word: u32, kind: FpInt) -> Result<i64, Trap> {
		let rm = match self.fp_rm(word) {
			Some(rm) => rm,
			None => return Err(Trap {
				trap_type: TrapType::IllegalInstruction,
				value: word as u64
			})
		};
		let r = fp_round(v, rm);
		let (lo, hi) = kind.bounds();
		let value = match r >= lo && r < hi {
			true => {
				if r != v {
					self.set_fcsr_nx();
				}
				match kind {
					FpInt::Lu => r as u64 as i64,
					_ => r as i64
				}
			},
			false => {
				self.set_fcsr_nv();
				let high = v.is_nan() || r >= hi;
				match kind {
					FpInt::W => if high { i32::MAX as i64 } else { i32::MIN as i64 },
					FpInt::Wu => if high { u32::MAX as i64 } else { 0 },
					FpInt::L => if high { i64::MAX } else { i64::MIN },
					FpInt::Lu => if high { -1 } else { 0 }
				}
			}
		};
		Ok(match kind {
			FpInt::W | FpInt::Wu => value as i32 as i64,
			_ => value
		})
	}

	/// A double arithmetic result: a NaN becomes the canonical NaN, raising
	/// NV when an input was a signaling NaN or none was a NaN at all (an
	/// invalid operation: 0/0, inf-inf, 0*inf, sqrt of a negative).
	fn fp_res_d(&mut self, r: f64, ins: &[f64]) -> f64 {
		if !r.is_nan() {
			return r;
		}
		if ins.iter().any(|&x| snan_d(x)) || !ins.iter().any(|x| x.is_nan()) {
			self.set_fcsr_nv();
		}
		f64::from_bits(FP_CANON_D)
	}

	/// fp_res_d for a single result, which it returns NaN-boxed.
	fn fp_res_s(&mut self, r: f32, ins: &[f32]) -> f64 {
		if !r.is_nan() {
			return s_box(r);
		}
		if ins.iter().any(|&x| snan_s(x)) || !ins.iter().any(|x| x.is_nan()) {
			self.set_fcsr_nv();
		}
		s_box(f32::from_bits(FP_CANON_S))
	}

	/// IEEE division (a / ±0 is a correctly signed infinity, 0/0 a NaN);
	/// DZ only for a finite nonzero dividend.
	fn fp_div_d(&mut self, a: f64, b: f64) -> f64 {
		if b == 0.0 && a.is_finite() && a != 0.0 {
			self.set_fcsr_dz();
		}
		self.fp_res_d(a / b, &[a, b])
	}

	fn fp_div_s(&mut self, a: f32, b: f32) -> f64 {
		if b == 0.0 && a.is_finite() && a != 0.0 {
			self.set_fcsr_dz();
		}
		self.fp_res_s(a / b, &[a, b])
	}

	/// The fused multiply-adds, one rounding: (±a)*b + (±c) — FMSUB is
	/// a*b-c, FNMSUB -(a*b)+c, FNMADD -(a*b)-c. inf*0 is invalid even when
	/// the addend is a quiet NaN.
	fn fp_fma_d(&mut self, a: f64, b: f64, c: f64, neg_prod: bool, neg_add: bool) -> f64 {
		let r = (if neg_prod { -a } else { a }).mul_add(b, if neg_add { -c } else { c });
		if (a.is_infinite() && b == 0.0) || (a == 0.0 && b.is_infinite()) {
			self.set_fcsr_nv();
		}
		self.fp_res_d(r, &[a, b, c])
	}

	fn fp_fma_s(&mut self, a: f32, b: f32, c: f32, neg_prod: bool, neg_add: bool) -> f64 {
		let r = (if neg_prod { -a } else { a }).mul_add(b, if neg_add { -c } else { c });
		if (a.is_infinite() && b == 0.0) || (a == 0.0 && b.is_infinite()) {
			self.set_fcsr_nv();
		}
		self.fp_res_s(r, &[a, b, c])
	}

	/// FMIN/FMAX (spec 2.2): both NaN -> the canonical NaN, one NaN -> the
	/// other operand, -0.0 below +0.0; NV for a signaling NaN input.
	fn fp_minmax_d(&mut self, a: f64, b: f64, max: bool) -> f64 {
		if snan_d(a) || snan_d(b) {
			self.set_fcsr_nv();
		}
		match (a.is_nan(), b.is_nan()) {
			(true, true) => f64::from_bits(FP_CANON_D),
			(true, false) => b,
			(false, true) => a,
			// equal: only ±0 differ, and the sign picks
			_ if a == b => if a.is_sign_negative() != max { a } else { b },
			_ => if (a < b) != max { a } else { b }
		}
	}

	fn fp_minmax_s(&mut self, a: f32, b: f32, max: bool) -> f64 {
		if snan_s(a) || snan_s(b) {
			self.set_fcsr_nv();
		}
		s_box(match (a.is_nan(), b.is_nan()) {
			(true, true) => f32::from_bits(FP_CANON_S),
			(true, false) => b,
			(false, true) => a,
			_ if a == b => if a.is_sign_negative() != max { a } else { b },
			_ => if (a < b) != max { a } else { b }
		})
	}

	/// FEQ (quiet: NV for signaling NaNs only) or FLT/FLE (signaling: NV for
	/// any NaN); a NaN operand compares false.
	fn fp_cmp_d(&mut self, a: f64, b: f64, signaling: bool) {
		if (signaling && (a.is_nan() || b.is_nan())) || snan_d(a) || snan_d(b) {
			self.set_fcsr_nv();
		}
	}

	fn fp_cmp_s(&mut self, a: f32, b: f32, signaling: bool) {
		if (signaling && (a.is_nan() || b.is_nan())) || snan_s(a) || snan_s(b) {
			self.set_fcsr_nv();
		}
	}

	/// FCVT.D.S: exact widening; a NaN becomes the canonical NaN (NV when
	/// it was signaling).
	fn fp_d_of_s(&mut self, a: f32) -> f64 {
		if !a.is_nan() {
			return a as f64;
		}
		if snan_s(a) {
			self.set_fcsr_nv();
		}
		f64::from_bits(FP_CANON_D)
	}

	/// FCVT.S.D: round to nearest-even, NaN-boxed; a NaN becomes the
	/// canonical NaN (NV when it was signaling).
	fn fp_s_of_d(&mut self, a: f64) -> f64 {
		if !a.is_nan() {
			return s_box(a as f32);
		}
		if snan_d(a) {
			self.set_fcsr_nv();
		}
		s_box(f32::from_bits(FP_CANON_S))
	}
}
// ===== end floating-point semantics =====

// risc-box patch: 161 = upstream's table + the AMOs it never had, appended
// at the END so every existing entry keeps its index (a BlockOp carries the
// index in `data`, and the baked AOT regions are keyed by a hash over it).
const INSTRUCTION_NUM: usize = 161;

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
			// risc-box patch (fp spec): canonical NaN, NV (fp_res_d)
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.f[f.rd] = cpu.fp_res_d(a + b, &[a, b]);
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
			// risc-box patch (fp spec): the input unboxed, a NaN canonical
			let a = s_unbox(cpu.f[f.rs1]);
			cpu.f[f.rd] = cpu.fp_d_of_s(a);
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
			// The register holds raw FP bits. Widening the rounded value back
			// to f64 makes FSW/FCVT.D.S read the low half of a double instead
			// of the single (1.0 became 0.0). Store a NaN-boxed single.
			// risc-box patch (fp spec): ... and a NaN as the canonical one.
			let a = cpu.f[f.rs1];
			cpu.f[f.rd] = cpu.fp_s_of_d(a);
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
			// risc-box patch (fp spec): and it always truncated: rm decides
			// the rounding (SpiderMonkey's Math.floor is fcvt.w.d RDN), and
			// NV/NX report invalid and inexact (fp_to_int).
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::W)?;
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
			// risc-box patch: NaN -> u32::MAX per spec (result sign-extended);
			// rounding per rm, NV/NX (fp_to_int)
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::Wu)?;
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
			// risc-box patch: NaN -> i64::MAX per spec; rounding per rm, NV/NX
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::L)?;
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
			// risc-box patch: NaN -> u64::MAX per spec; rounding per rm, NV/NX
			let a = cpu.f[f.rs1];
			cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::Lu)?;
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
			// risc-box patch (fp spec): upstream returned +inf for ANY zero
			// divisor (-0.0 == 0.0, so its -0.0 arm never ran): 0/0 must be
			// the canonical NaN (NV), -1/0 -inf; DZ only for a finite
			// nonzero dividend (fp_div_d)
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.f[f.rd] = cpu.fp_div_d(a, b);
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
			// risc-box patch (fp spec): NV for a signaling NaN
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.fp_cmp_d(a, b, false);
			cpu.x[f.rd] = match a == b {
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
			// risc-box patch (fp spec): NV for any NaN
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.fp_cmp_d(a, b, true);
			cpu.x[f.rd] = match a <= b {
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
			// risc-box patch (fp spec): NV for any NaN
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.fp_cmp_d(a, b, true);
			cpu.x[f.rd] = match a < b {
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
			// risc-box patch (fp spec): NaN-boxed (upstream sign-extended,
			// which left a positive single unboxed)
			cpu.f[f.rd] = match cpu.mmu.load_word(cpu.x[f.rs1].wrapping_add(f.imm) as u64) {
				Ok(data) => f64::from_bits(FP_BOX | data as u64),
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
			let f = parse_format_r2(word);
			// risc-box patch (fp spec): FUSED (one rounding), canonical NaN
			let (a, b, c) = (cpu.f[f.rs1], cpu.f[f.rs2], cpu.f[f.rs3]);
			cpu.f[f.rd] = cpu.fp_fma_d(a, b, c, false, false);
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
			// risc-box patch (fp spec): fused a*b-c
			let (a, b, c) = (cpu.f[f.rs1], cpu.f[f.rs2], cpu.f[f.rs3]);
			cpu.f[f.rd] = cpu.fp_fma_d(a, b, c, false, true);
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
			// risc-box patch (fp spec): fused -(a*b)-c
			let (a, b, c) = (cpu.f[f.rs1], cpu.f[f.rs2], cpu.f[f.rs3]);
			cpu.f[f.rd] = cpu.fp_fma_d(a, b, c, true, true);
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
			// risc-box patch (fp spec): Rust's min leaves ±0 unordered
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.f[f.rd] = cpu.fp_minmax_d(a, b, false);
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
			// risc-box patch (fp spec): spec min/max (fp_minmax_d)
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.f[f.rd] = cpu.fp_minmax_d(a, b, true);
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
			// risc-box patch (fp spec): canonical NaN, NV
			let a = cpu.f[f.rs1];
			cpu.f[f.rd] = cpu.fp_res_d(a.sqrt(), &[a]);
			Ok(())
		},
		disassemble: dump_format_r
	},
	Instruction {
		mask: 0xfe00007f,
		data: 0x12000053,
		name: "FMUL.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			// risc-box patch (fp spec): canonical NaN, NV
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.f[f.rd] = cpu.fp_res_d(a * b, &[a, b]);
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
			// risc-box patch (fp spec): NaN-boxed
			cpu.f[f.rd] = f64::from_bits(FP_BOX | cpu.x[f.rs1] as u32 as u64);
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
			// risc-box patch (fp spec): fused -(a*b)+c
			let (a, b, c) = (cpu.f[f.rs1], cpu.f[f.rs2], cpu.f[f.rs3]);
			cpu.f[f.rd] = cpu.fp_fma_d(a, b, c, true, false);
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
			// risc-box patch (fp spec): canonical NaN, NV
			let (a, b) = (cpu.f[f.rs1], cpu.f[f.rs2]);
			cpu.f[f.rd] = cpu.fp_res_d(a - b, &[a, b]);
			Ok(())
		},
		disassemble: dump_format_r
	},
		// ===== risc-box patch: the single-precision (RV64F) arithmetic set =====
		// Upstream implemented the full DOUBLE (.D) family but almost none of the
		// SINGLE (.S) one — only FCVT.D.S/FCVT.S.D + the FMV.{X.W,W.X} moves. Xorg
		// and glibc use single-precision floats constantly (window coordinates,
		// libm float paths), so the very first X screen setup SIGILL'd on FSGNJ.S
		// (word 0x20e705d3). fmt bits [26:25]=00 keep these distinct from the .D
		// encodings (fmt=01) that share the low opcode.
		// risc-box patch (fp spec): a single lives NaN-boxed in the register.
		// Every op here reads its inputs with s_unbox (an improperly boxed
		// register is the canonical NaN) and writes its result with s_box /
		// fp_res_s (boxed; arithmetic NaNs canonical, NV per IEEE).
		Instruction {
			mask: 0xfe00007f, data: 0x00000053, name: "FADD.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.f[f.rd] = cpu.fp_res_s(a + b, &[a, b]);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00007f, data: 0x08000053, name: "FSUB.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.f[f.rd] = cpu.fp_res_s(a - b, &[a, b]);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00007f, data: 0x10000053, name: "FMUL.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.f[f.rd] = cpu.fp_res_s(a * b, &[a, b]);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00007f, data: 0x18000053, name: "FDIV.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.f[f.rd] = cpu.fp_div_s(a, b);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0x58000053, name: "FSQRT.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				cpu.f[f.rd] = cpu.fp_res_s(a.sqrt(), &[a]);
				Ok(())
			}, disassemble: dump_format_r
		},
		// sign injection is bit-exact (no canonicalization) on the unboxed
		// singles
		Instruction {
			mask: 0xfe00707f, data: 0x20000053, name: "FSGNJ.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let r1 = s_unbox(cpu.f[f.rs1]).to_bits();
				let r2 = s_unbox(cpu.f[f.rs2]).to_bits();
				cpu.f[f.rd] = s_box(f32::from_bits((r2 & 0x80000000) | (r1 & 0x7fffffff)));
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x20001053, name: "FSGNJN.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let r1 = s_unbox(cpu.f[f.rs1]).to_bits();
				let r2 = s_unbox(cpu.f[f.rs2]).to_bits();
				cpu.f[f.rd] = s_box(f32::from_bits((!r2 & 0x80000000) | (r1 & 0x7fffffff)));
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x20002053, name: "FSGNJX.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let r1 = s_unbox(cpu.f[f.rs1]).to_bits();
				let r2 = s_unbox(cpu.f[f.rs2]).to_bits();
				cpu.f[f.rd] = s_box(f32::from_bits(((r1 ^ r2) & 0x80000000) | (r1 & 0x7fffffff)));
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x28000053, name: "FMIN.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.f[f.rd] = cpu.fp_minmax_s(a, b, false);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0x28001053, name: "FMAX.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.f[f.rd] = cpu.fp_minmax_s(a, b, true);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfe00707f, data: 0xa0002053, name: "FEQ.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.fp_cmp_s(a, b, false);
				cpu.x[f.rd] = (a == b) as i64;
				Ok(())
			}, disassemble: dump_empty
		},
		Instruction {
			mask: 0xfe00707f, data: 0xa0001053, name: "FLT.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.fp_cmp_s(a, b, true);
				cpu.x[f.rd] = (a < b) as i64;
				Ok(())
			}, disassemble: dump_empty
		},
		Instruction {
			mask: 0xfe00707f, data: 0xa0000053, name: "FLE.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				cpu.fp_cmp_s(a, b, true);
				cpu.x[f.rd] = (a <= b) as i64;
				Ok(())
			}, disassemble: dump_empty
		},
		Instruction {
			mask: 0xfff0707f, data: 0xe0001053, name: "FCLASS.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let bits = s_unbox(cpu.f[f.rs1]).to_bits();
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
		// float -> int: the single widened exactly to a double, then
		// fp_to_int's rounding (rm), saturation and flags. Upstream's `as`
		// truncated always and mapped NaN to 0.
		Instruction {
			mask: 0xfff0007f, data: 0xc0000053, name: "FCVT.W.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]) as f64;
				cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::W)?;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0100053, name: "FCVT.WU.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]) as f64;
				cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::Wu)?;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0200053, name: "FCVT.L.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]) as f64;
				cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::L)?;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xc0300053, name: "FCVT.LU.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				let a = s_unbox(cpu.f[f.rs1]) as f64;
				cpu.x[f.rd] = cpu.fp_to_int(a, word, FpInt::Lu)?;
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0000053, name: "FCVT.S.W",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = s_box(cpu.x[f.rs1] as i32 as f32);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0100053, name: "FCVT.S.WU",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = s_box(cpu.x[f.rs1] as u32 as f32);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0200053, name: "FCVT.S.L",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = s_box(cpu.x[f.rs1] as i64 as f32);
				Ok(())
			}, disassemble: dump_format_r
		},
		Instruction {
			mask: 0xfff0007f, data: 0xd0300053, name: "FCVT.S.LU",
			operation: |cpu, word, _address| {
				let f = parse_format_r(word);
				cpu.f[f.rd] = s_box(cpu.x[f.rs1] as u64 as f32);
				Ok(())
			}, disassemble: dump_format_r
		},
		// fused (one rounding), as the spec defines them: FMSUB a*b-c,
		// FNMSUB -(a*b)+c, FNMADD -(a*b)-c
		Instruction {
			mask: 0x0600007f, data: 0x00000043, name: "FMADD.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				let c = s_unbox(cpu.f[f.rs3]);
				cpu.f[f.rd] = cpu.fp_fma_s(a, b, c, false, false);
				Ok(())
			}, disassemble: dump_format_r2
		},
		Instruction {
			mask: 0x0600007f, data: 0x00000047, name: "FMSUB.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				let c = s_unbox(cpu.f[f.rs3]);
				cpu.f[f.rd] = cpu.fp_fma_s(a, b, c, false, true);
				Ok(())
			}, disassemble: dump_format_r2
		},
		Instruction {
			mask: 0x0600007f, data: 0x0000004b, name: "FNMSUB.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				let c = s_unbox(cpu.f[f.rs3]);
				cpu.f[f.rd] = cpu.fp_fma_s(a, b, c, true, false);
				Ok(())
			}, disassemble: dump_format_r2
		},
		Instruction {
			mask: 0x0600007f, data: 0x0000004f, name: "FNMADD.S",
			operation: |cpu, word, _address| {
				let f = parse_format_r2(word);
				let a = s_unbox(cpu.f[f.rs1]);
				let b = s_unbox(cpu.f[f.rs2]);
				let c = s_unbox(cpu.f[f.rs3]);
				cpu.f[f.rd] = cpu.fp_fma_s(a, b, c, true, true);
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
	},	// risc-box patch: the AMOs upstream never implemented. Without them the
	// guest SIGILLs on the first one: Rust's AtomicX::fetch_xor/fetch_min/
	// fetch_max compile straight to these, and tokio's task state machine
	// uses fetch_xor, so every tokio program died with "Illegal
	// instruction" (rustup's default downloader among them). Same shape as
	// the AMOs above: load, compute from x[rs2] and the old value, store,
	// rd = old (a W result sign-extended). One hart, so no atomicity to keep.
	Instruction {
		mask: 0xf800707f,
		data: 0x2000302f,
		name: "AMOXOR.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i64 = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i64;
			let new: i64 = src ^ tmp;
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, new as u64) {
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
		data: 0x2000202f,
		name: "AMOXOR.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i32 = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i32;
			let new: i32 = src ^ tmp;
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, new as u32) {
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
		data: 0x8000302f,
		name: "AMOMIN.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i64 = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i64;
			let new: i64 = src.min(tmp);
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, new as u64) {
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
		data: 0x8000202f,
		name: "AMOMIN.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i32 = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i32;
			let new: i32 = src.min(tmp);
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, new as u32) {
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
		data: 0xa000302f,
		name: "AMOMAX.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i64 = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i64;
			let new: i64 = src.max(tmp);
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, new as u64) {
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
		data: 0xa000202f,
		name: "AMOMAX.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i32 = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i32;
			let new: i32 = src.max(tmp);
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, new as u32) {
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
		data: 0xc000302f,
		name: "AMOMINU.D",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i64 = match cpu.mmu.load_doubleword(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i64,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i64;
			let new: i64 = (src as u64).min(tmp as u64) as i64;
			match cpu.mmu.store_doubleword(cpu.x[f.rs1] as u64, new as u64) {
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
		data: 0xc000202f,
		name: "AMOMINU.W",
		operation: |cpu, word, _address| {
			let f = parse_format_r(word);
			let tmp: i32 = match cpu.mmu.load_word(cpu.x[f.rs1] as u64) {
				Ok(data) => data as i32,
				Err(e) => return Err(e)
			};
			let src = cpu.x[f.rs2] as i32;
			let new: i32 = (src as u32).min(tmp as u32) as i32;
			match cpu.mmu.store_word(cpu.x[f.rs1] as u64, new as u32) {
				Ok(()) => {},
				Err(e) => return Err(e)
			};
			cpu.x[f.rd] = tmp as i64;
			Ok(())
		},
		disassemble: dump_format_r
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

#[cfg(feature = "jit")]
/// risc-box patch (jit): the INSTRUCTIONS entry a non-hot (kind 0) op runs —
/// exactly the closure exec_op dispatches to, so the translator keys on the
/// interpreter's own decode.
pub(crate) fn op_name(op: &BlockOp) -> &'static str {
	INSTRUCTIONS.get((op.data & !ICACHE_LEN4) as usize).map_or("", |i| i.name)
}

/// Tests: the BlockOp the predecoder would build for `word` (any kind).
#[cfg(test)]
pub(crate) fn decode_op_for_test(cpu: &Cpu, word: u32) -> BlockOp {
	let index = cpu.decode_and_get_instruction_index(word).expect("decodes");
	let (kind, rd, rs1, rs2, imm) = classify_hot(INSTRUCTIONS[index].name, word);
	BlockOp { imm, word, data: index as u16 | ICACHE_LEN4, kind, rd, rs1, rs2, len: 4, _pad: 0 }
}

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

	/// The AMOs upstream never had decode (no more SIGILL) and do what the
	/// spec says: memory gets f(rs2, old), rd gets old (a .W old value
	/// sign-extended), signed vs unsigned comparisons where it matters.
	/// Hand-computed expectations, independent of the JIT tests (which
	/// only compare the translator against these closures).
	#[test]
	fn the_added_amos_decode_and_follow_the_spec() {
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(65536);
		let at = DRAM_BASE + 0x100;
		// (name, funct5, .W?, old in memory, x[rs2], stored, rd)
		let cases: &[(&str, u32, bool, u64, i64, u64, i64)] = &[
			("AMOXOR.D", 0b00100, false, 0xff00, 0x0ff0, 0xf0f0, 0xff00),
			("AMOXOR.W", 0b00100, true, 0x8000_0001, 3, 0x8000_0002, 0xffff_ffff_8000_0001u64 as i64),
			("AMOMIN.D", 0b10000, false, 5, -3, -3i64 as u64, 5),
			("AMOMIN.W", 0b10000, true, 0xffff_ffff, 1, 0xffff_ffff, -1),
			("AMOMAX.D", 0b10100, false, (-9i64) as u64, -2, (-2i64) as u64, -9),
			("AMOMAX.W", 0b10100, true, 0xffff_fffe, 0x1_0000_0001, 1, -2),
			("AMOMINU.D", 0b11000, false, 7, -1, 7, 7),
			("AMOMINU.W", 0b11000, true, 0xffff_fff0, 0x5, 5, 0xffff_fff0u32 as i32 as i64),
		];
		for &(name, funct5, w, old, src, stored, rd) in cases {
			let word = funct5 << 27 | if w { 0x2000 } else { 0x3000 } | 0x2f | 5 << 7 | 6 << 15 | 7 << 20;
			let index = cpu.decode_and_get_instruction_index(word).unwrap_or_else(|_| panic!("{} decodes", name));
			assert_eq!(INSTRUCTIONS[index].name, name);
			let _ = cpu.get_mut_mmu().store_doubleword(at, 0);
			let _ = match w {
				true => cpu.get_mut_mmu().store_word(at, old as u32),
				false => cpu.get_mut_mmu().store_doubleword(at, old),
			};
			cpu.x[6] = at as i64;
			cpu.x[7] = src;
			assert!((INSTRUCTIONS[index].operation)(&mut cpu, word, 0).is_ok(), "{}", name);
			let mem = match w {
				true => cpu.get_mut_mmu().load_word(at).ok().unwrap() as u64,
				false => cpu.get_mut_mmu().load_doubleword(at).ok().unwrap(),
			};
			assert_eq!((mem, cpu.x[5]), (stored, rd), "{}", name);
		}
	}

	/// The word of INSTRUCTIONS entry `name`, with rd/rm/rs1/rs2/rs3 filled
	/// in wherever its mask leaves them free.
	fn fp_word(name: &str, rd: u32, rs1: u32, rs2: u32, rs3: u32, rm: u32) -> u32 {
		let i = INSTRUCTIONS.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{}", name));
		let fields = rd << 7 | rm << 12 | rs1 << 15 | rs2 << 20 | rs3 << 27;
		i.data | (fields & !i.mask)
	}

	/// Run float op `name` (rd 5, rs1 6, rs2 7, rs3 8) from f6/f7/f8 = the
	/// given bits, x6 = `x6`, frm = `frm`, fflags clear — through its table
	/// closure AND through exec_op (the hot arm, for the hot ones), which
	/// must agree. Returns (f5 bits, x5, fflags), or None on a trap.
	fn fp_run(name: &str, rm: u32, f: [u64; 3], x6: i64, frm: u64) -> Option<(u64, i64, u64)> {
		let word = fp_word(name, 5, 6, 7, 8, rm);
		let mut out = Vec::new();
		for hot in [false, true] {
			let mut cpu = create_cpu();
			for i in 0..3 {
				cpu.f[6 + i] = f64::from_bits(f[i]);
			}
			cpu.x[6] = x6;
			cpu.csr[CSR_FCSR_ADDRESS as usize] = frm << 5;
			let index = cpu.decode_and_get_instruction_index(word).unwrap_or_else(|_| panic!("{} decodes", name));
			assert_eq!(INSTRUCTIONS[index].name, name);
			let result = match hot {
				false => (INSTRUCTIONS[index].operation)(&mut cpu, word, 0),
				true => {
					let op = decode_op_for_test(&cpu, word);
					cpu.exec_op(&op, 0)
				}
			};
			out.push(match result {
				Ok(()) => Some((cpu.f[5].to_bits(), cpu.x[5], cpu.csr[CSR_FCSR_ADDRESS as usize] & 0x1f)),
				Err(Trap { trap_type: TrapType::IllegalInstruction, value }) => {
					assert_eq!(value, word as u64, "{}: tval is the word", name);
					None
				},
				Err(_) => panic!("{}: unexpected trap", name),
			});
		}
		assert_eq!(out[0], out[1], "{}: table closure vs exec_op", name);
		out[0]
	}

	const NV: u64 = FFLAG_NV;
	const DZ: u64 = FFLAG_DZ;
	const NX: u64 = FFLAG_NX;
	fn d(v: f64) -> u64 {
		v.to_bits()
	}
	fn s(v: f32) -> u64 {
		FP_BOX | v.to_bits() as u64
	}

	/// risc-box patch (fp spec): FDIV, NaN canonicalization and the
	/// exception flags, hand-computed from the spec (the JIT tests only
	/// compare the translator against the interpreter).
	#[test]
	fn float_arithmetic_follows_the_spec() {
		let canon_d = FP_CANON_D;
		let canon_s = FP_BOX | FP_CANON_S as u64;
		let snan_d = 0x7ff0_0000_0000_0001u64;
		let neg_qnan_d = 0xfff8_0000_0000_0123u64;
		// (op, rs1, rs2, f5, fflags)
		let cases: &[(&str, u64, u64, u64, u64)] = &[
			// FDIV.D: upstream gave +inf for every zero divisor
			("FDIV.D", d(0.0), d(0.0), canon_d, NV),
			("FDIV.D", d(-1.0), d(0.0), d(f64::NEG_INFINITY), DZ),
			("FDIV.D", d(1.0), d(-0.0), d(f64::NEG_INFINITY), DZ),
			("FDIV.D", d(-1.0), d(-0.0), d(f64::INFINITY), DZ),
			("FDIV.D", d(f64::INFINITY), d(0.0), d(f64::INFINITY), 0),
			("FDIV.D", d(f64::INFINITY), d(f64::INFINITY), canon_d, NV),
			("FDIV.D", d(1.0), d(4.0), d(0.25), 0),
			("FDIV.D", neg_qnan_d, d(0.0), canon_d, 0),
			("FDIV.S", s(0.0), s(-0.0), canon_s, NV),
			("FDIV.S", s(-3.0), s(0.0), s(f32::NEG_INFINITY), DZ),
			// NaN results are canonical; NV only for invalid ops and sNaNs
			("FADD.D", d(f64::INFINITY), d(f64::NEG_INFINITY), canon_d, NV),
			("FADD.D", neg_qnan_d, d(1.0), canon_d, 0),
			("FADD.D", snan_d, d(1.0), canon_d, NV),
			("FSUB.D", d(f64::INFINITY), d(f64::INFINITY), canon_d, NV),
			("FMUL.D", d(0.0), d(f64::NEG_INFINITY), canon_d, NV),
			("FMUL.D", d(-2.0), d(3.0), d(-6.0), 0),
			("FSQRT.D", d(-1.0), 0, canon_d, NV),
			("FSQRT.D", d(-0.0), 0, d(-0.0), 0),
			("FADD.S", s(f32::INFINITY), s(f32::NEG_INFINITY), canon_s, NV),
			("FMUL.S", FP_BOX | 0xffc0_0001, s(2.0), canon_s, 0),
			("FADD.S", FP_BOX | 0x7f80_0001, s(2.0), canon_s, NV),
			// an improperly boxed single input is the canonical NaN (quiet)
			("FADD.S", 0x0000_0000_3f80_0000, s(1.0), canon_s, 0),
			("FSQRT.S", s(4.0), 0, s(2.0), 0),
			// single results are NaN-boxed
			("FADD.S", s(1.0), s(2.0), s(3.0), 0),
			// sign injection is bit-exact: no canonicalization, payloads kept
			("FSGNJ.D", neg_qnan_d, d(1.0), 0x7ff8_0000_0000_0123, 0),
			("FSGNJN.D", snan_d, d(1.0), 0x8000_0000_0000_0000 | snan_d, 0),
			("FSGNJ.S", FP_BOX | 0x7f80_0001, s(-1.0), FP_BOX | 0xff80_0001, 0),
			("FSGNJ.S", 0x0000_0000_3f80_0000, s(-1.0), FP_BOX | 0xffc0_0000, 0),
			// FMIN/FMAX: -0.0 < +0.0; one NaN gives the other operand
			("FMIN.D", d(-0.0), d(0.0), d(-0.0), 0),
			("FMIN.D", d(0.0), d(-0.0), d(-0.0), 0),
			("FMAX.D", d(-0.0), d(0.0), d(0.0), 0),
			("FMAX.D", d(0.0), d(-0.0), d(0.0), 0),
			("FMIN.D", neg_qnan_d, d(2.0), d(2.0), 0),
			("FMAX.D", d(-3.0), neg_qnan_d, d(-3.0), 0),
			("FMIN.D", neg_qnan_d, neg_qnan_d, canon_d, 0),
			("FMIN.D", snan_d, d(5.0), d(5.0), NV),
			("FMIN.D", d(1.0), d(2.0), d(1.0), 0),
			("FMAX.D", d(1.0), d(2.0), d(2.0), 0),
			("FMIN.S", s(-0.0), s(0.0), s(-0.0), 0),
			("FMAX.S", s(0.0), s(-0.0), s(0.0), 0),
			("FMAX.S", 0x0000_0000_3f80_0000, s(7.0), s(7.0), 0),
			// widening/narrowing: NaNs canonical, NV for signaling
			("FCVT.D.S", FP_BOX | 0xffc0_0001, 0, canon_d, 0),
			("FCVT.D.S", FP_BOX | 0x7f80_0001, 0, canon_d, NV),
			("FCVT.D.S", s(1.5), 0, d(1.5), 0),
			("FCVT.D.S", 0x0000_0000_3fc0_0000, 0, canon_d, 0),
			("FCVT.S.D", neg_qnan_d, 0, canon_s, 0),
			("FCVT.S.D", snan_d, 0, canon_s, NV),
			("FCVT.S.D", d(-2.5), 0, s(-2.5), 0),
		];
		for &(name, a, b, want, flags) in cases {
			let got = fp_run(name, 0, [a, b, 0], 0, 0);
			assert_eq!(got.map(|g| (g.0, g.2)), Some((want, flags)), "{} {:#x} {:#x}", name, a, b);
		}
		// comparisons: false on NaN; FEQ is quiet (NV for sNaN only), FLT/FLE
		// signal on any NaN
		let cmps: &[(&str, u64, u64, i64, u64)] = &[
			("FEQ.D", neg_qnan_d, d(1.0), 0, 0),
			("FEQ.D", snan_d, d(1.0), 0, NV),
			("FEQ.D", d(-0.0), d(0.0), 1, 0),
			("FLT.D", neg_qnan_d, d(1.0), 0, NV),
			("FLE.D", d(1.0), d(1.0), 1, 0),
			("FLT.D", d(-0.0), d(0.0), 0, 0),
			("FEQ.S", s(1.0), 0x0000_0000_3f80_0000, 0, 0),
			("FLE.S", s(1.0), 0x0000_0000_3f80_0000, 0, NV),
			("FLT.S", s(1.0), s(2.0), 1, 0),
		];
		for &(name, a, b, want, flags) in cmps {
			let got = fp_run(name, 0, [a, b, 0], 0, 0);
			assert_eq!(got.map(|g| (g.1, g.2)), Some((want, flags)), "{} {:#x} {:#x}", name, a, b);
		}
		// FCLASS.S of an improperly boxed register: the canonical (quiet) NaN
		assert_eq!(fp_run("FCLASS.S", 0, [0x0000_0000_3f80_0000, 0, 0], 0, 0).map(|g| g.1), Some(1 << 9));
		assert_eq!(fp_run("FCLASS.S", 0, [s(-0.0), 0, 0], 0, 0).map(|g| g.1), Some(1 << 3));
		// FMV.X.W takes the low 32 bits raw (sign-extended), boxed or not;
		// FMV.W.X boxes
		assert_eq!(fp_run("FMV.X.W", 0, [0x0000_0000_bf80_0000, 0, 0], 0, 0).map(|g| g.1),
			Some(0xffff_ffff_bf80_0000u64 as i64));
		assert_eq!(fp_run("FMV.W.X", 0, [0, 0, 0], 0x1234_5678_3f80_0000, 0).map(|g| g.0),
			Some(0xffff_ffff_3f80_0000));
		// int -> single results are boxed
		assert_eq!(fp_run("FCVT.S.W", 0, [0, 0, 0], 3, 0).map(|g| g.0), Some(s(3.0)));
	}

	/// risc-box patch (fp spec): FLW boxes the single it loads (upstream
	/// sign-extended it: 1.0f became 0x000000003f800000, which every single
	/// op now reads as NaN), through the table closure and the hot arm.
	#[test]
	fn flw_nan_boxes_its_single() {
		for &(bits, want) in &[(0x3f80_0000u32, 0xffff_ffff_3f80_0000u64), (0xbf80_0000, 0xffff_ffff_bf80_0000),
			(0x7f80_0001, 0xffff_ffff_7f80_0001)]
		{
			for hot in [false, true] {
				let mut cpu = create_cpu();
				cpu.get_mut_mmu().init_memory(65536);
				let at = DRAM_BASE + 0x100;
				cpu.get_mut_mmu().store_word(at, bits).ok().unwrap();
				cpu.x[6] = at as i64;
				let word = fp_word("FLW", 5, 6, 0, 0, 2); // flw f5, 0(x6)
				let index = cpu.decode_and_get_instruction_index(word).ok().unwrap();
				assert_eq!(INSTRUCTIONS[index].name, "FLW");
				let ok = match hot {
					false => (INSTRUCTIONS[index].operation)(&mut cpu, word, 0).is_ok(),
					true => {
						let op = decode_op_for_test(&cpu, word);
						assert_eq!(op.kind, HOT_FLW);
						cpu.exec_op(&op, 0).is_ok()
					}
				};
				assert!(ok);
				assert_eq!(cpu.f[5].to_bits(), want, "{:#x} hot {}", bits, hot);
			}
		}
	}

	/// risc-box patch (fp spec): float -> int conversions round per rm
	/// (DYN reads frm), saturate, set NV/NX; reserved rounding modes are
	/// illegal. SpiderMonkey's Math.floor/ceil/round are fcvt.w.d with
	/// RDN/RUP/RMM and test NV|NX to see whether the double was an int32.
	#[test]
	fn float_to_int_conversions_follow_the_spec() {
		const RNE: u32 = 0;
		const RTZ: u32 = 1;
		const RDN: u32 = 2;
		const RUP: u32 = 3;
		const RMM: u32 = 4;
		const DYN: u32 = 7;
		let nan = 0x7ff8_0000_0000_0000u64;
		// (op, rm, input bits, x5, fflags)
		let cases: &[(&str, u32, u64, i64, u64)] = &[
			("FCVT.W.D", RDN, d(-1.5), -2, NX),
			("FCVT.W.D", RUP, d(-1.5), -1, NX),
			("FCVT.W.D", RNE, d(-1.5), -2, NX),
			("FCVT.W.D", RTZ, d(-1.5), -1, NX),
			("FCVT.W.D", RMM, d(-1.5), -2, NX),
			("FCVT.W.D", RNE, d(2.5), 2, NX),
			("FCVT.W.D", RMM, d(2.5), 3, NX),
			("FCVT.W.D", RNE, d(3.5), 4, NX),
			("FCVT.W.D", RMM, d(0.49999999999999994), 0, NX),
			("FCVT.W.D", RMM, d(-0.5), -1, NX),
			("FCVT.W.D", RDN, d(7.0), 7, 0),
			("FCVT.W.D", RTZ, d(-0.0), 0, 0),
			("FCVT.W.D", RNE, nan, 0x7fff_ffff, NV),
			("FCVT.W.D", RNE, 0xfff8_0000_0000_0000, 0x7fff_ffff, NV),
			("FCVT.W.D", RTZ, d(f64::NEG_INFINITY), i32::MIN as i64, NV),
			("FCVT.W.D", RTZ, d(2147483647.9), 2147483647, NX),
			("FCVT.W.D", RNE, d(2147483647.5), 0x7fff_ffff, NV),
			("FCVT.W.D", RNE, d(-2147483648.5), i32::MIN as i64, NX),
			("FCVT.W.D", RMM, d(-2147483648.5), i32::MIN as i64, NV),
			("FCVT.WU.D", RTZ, d(-0.5), 0, NX),
			("FCVT.WU.D", RTZ, d(-1.0), 0, NV),
			("FCVT.WU.D", RNE, d(4294967295.0), -1, 0),
			("FCVT.WU.D", RNE, d(4294967296.0), -1, NV),
			("FCVT.WU.D", RNE, nan, -1, NV),
			("FCVT.WU.D", RUP, d(2147483647.5), 0xffff_ffff_8000_0000u64 as i64, NX),
			("FCVT.L.D", RNE, d(9223372036854775808.0), i64::MAX, NV),
			("FCVT.L.D", RNE, d(-9223372036854775808.0), i64::MIN, 0),
			("FCVT.L.D", RDN, d(-0.25), -1, NX),
			("FCVT.L.D", RNE, nan, i64::MAX, NV),
			("FCVT.LU.D", RNE, d(18446744073709549568.0), -2048, 0),
			("FCVT.LU.D", RNE, d(f64::INFINITY), -1, NV),
			("FCVT.LU.D", RUP, d(-0.5), 0, NX),
			("FCVT.LU.D", RDN, d(-0.5), 0, NV),
			("FCVT.W.S", RDN, s(-1.5), -2, NX),
			("FCVT.W.S", RUP, s(-1.5), -1, NX),
			("FCVT.W.S", RMM, s(2.5), 3, NX),
			// upstream mapped a NaN single to 0
			("FCVT.W.S", RTZ, FP_BOX | 0x7fc0_0000, 0x7fff_ffff, NV),
			// an improperly boxed single is the canonical NaN
			("FCVT.W.S", RTZ, 0x0000_0000_3f80_0000, 0x7fff_ffff, NV),
			("FCVT.WU.S", RTZ, s(-3.0), 0, NV),
			("FCVT.L.S", RUP, s(-0.5), 0, NX),
			("FCVT.LU.S", RNE, s(1e10), 10000000000, 0),
		];
		for &(name, rm, a, want, flags) in cases {
			let got = fp_run(name, rm, [a, 0, 0], 0, 0);
			assert_eq!(got.map(|g| (g.1, g.2)), Some((want, flags)), "{} rm {} {:#x}", name, rm, a);
		}
		// DYN reads frm
		for &(frm, want) in &[(RNE as u64, -2i64), (RTZ as u64, -1), (RDN as u64, -2), (RUP as u64, -1), (RMM as u64, -2)] {
			let got = fp_run("FCVT.W.D", DYN, [d(-1.5), 0, 0], 0, frm);
			assert_eq!(got.map(|g| g.1), Some(want), "DYN frm {}", frm);
		}
		// the reserved rounding modes: rm 5/6, or DYN with frm 5-7, trap
		for &(rm, frm) in &[(5u32, 0u64), (6, 0), (DYN, 5), (DYN, 6), (DYN, 7)] {
			assert_eq!(fp_run("FCVT.W.D", rm, [d(1.0), 0, 0], 0, frm), None, "rm {} frm {}", rm, frm);
			assert_eq!(fp_run("FCVT.LU.S", rm, [s(1.0), 0, 0], 0, frm), None, "rm {} frm {}", rm, frm);
		}
	}

	/// risc-box patch (fp spec): the multiply-adds are FUSED - one rounding.
	/// a = 1 + 2^-30, b = 1 - 2^-30: a*b = 1 - 2^-60 exactly, which rounds
	/// to 1.0 on its own, so unfused a*b - 1 is 0 while fused is -2^-60.
	/// (GCC and Clang contract a*b+c into fmadd on riscv64: libm's pow()
	/// came out wrong unfused, 2**53+1 printed 9007199254740996 in node.)
	#[test]
	fn multiply_adds_are_fused() {
		let a = 1.0 + 2f64.powi(-30);
		let b = 1.0 - 2f64.powi(-30);
		let e = 2f64.powi(-60);
		assert_eq!(a * b - 1.0, 0.0, "the unfused reference really differs");
		// (op, rs3, result): FMADD a*b+c, FMSUB a*b-c, FNMSUB -(a*b)+c,
		// FNMADD -(a*b)-c
		for &(name, c, want) in &[("FMADD.D", -1.0, -e), ("FMSUB.D", 1.0, -e), ("FNMSUB.D", 1.0, e),
			("FNMADD.D", -1.0, e)]
		{
			let got = fp_run(name, 0, [d(a), d(b), d(c)], 0, 0);
			assert_eq!(got.map(|g| (g.0, g.2)), Some((d(want), 0)), "{}", name);
		}
		let a = 1.0f32 + 2f32.powi(-13);
		let b = 1.0f32 - 2f32.powi(-13);
		let e = 2f32.powi(-26);
		assert_eq!(a * b - 1.0, 0.0);
		for &(name, c, want) in &[("FMADD.S", -1.0f32, -e), ("FMSUB.S", 1.0, -e), ("FNMSUB.S", 1.0, e),
			("FNMADD.S", -1.0, e)]
		{
			let got = fp_run(name, 0, [s(a), s(b), s(c)], 0, 0);
			assert_eq!(got.map(|g| (g.0, g.2)), Some((s(want), 0)), "{}", name);
		}
		// more fused-only results: (1+2^-27)^2 - 1 = 2^-26 + 2^-54, and an
		// exact x*x - x*x residual (the TwoProduct error term)
		let x = 1.0 + 2f64.powi(-27);
		let got = fp_run("FMADD.D", 0, [d(x), d(x), d(-1.0)], 0, 0);
		assert_eq!(got.map(|g| g.0), Some(d(2f64.powi(-26) + 2f64.powi(-54))));
		let p = x * x;
		let got = fp_run("FMSUB.D", 0, [d(x), d(x), d(p)], 0, 0);
		assert_eq!(got.map(|g| g.0), Some(d(2f64.powi(-54))));
		// NaNs: canonical; inf*0 is invalid even with a quiet NaN addend
		let canon = FP_CANON_D;
		assert_eq!(fp_run("FMADD.D", 0, [d(f64::INFINITY), d(0.0), 0xfff8_0000_0000_0001], 0, 0),
			Some((canon, 0, NV)));
		assert_eq!(fp_run("FMADD.D", 0, [d(2.0), d(3.0), 0xfff8_0000_0000_0001], 0, 0),
			Some((canon, 0, 0)));
		assert_eq!(fp_run("FMADD.D", 0, [d(f64::INFINITY), d(1.0), d(f64::NEG_INFINITY)], 0, 0),
			Some((canon, 0, NV)));
		assert_eq!(fp_run("FNMSUB.S", 0, [s(f32::INFINITY), s(0.0), s(1.0)], 0, 0),
			Some((FP_BOX | FP_CANON_S as u64, 0, NV)));
		// FNMADD/FNMSUB negate the product, not the result: zero signs
		assert_eq!(fp_run("FNMADD.D", 0, [d(0.0), d(1.0), d(0.0)], 0, 0).map(|g| g.0), Some(d(-0.0)));
		assert_eq!(fp_run("FNMSUB.D", 0, [d(0.0), d(1.0), d(0.0)], 0, 0).map(|g| g.0), Some(d(0.0)));
	}

	/// risc-box patch (per-page code generations): a store into one code page retires only the blocks decoded
	/// from THAT page. Pages A and B hold code that jumps between them (both decoded, both marked); a store into
	/// B moves the epoch and B's generation, A's cached block stays valid; a store into A then retires A's too.
	#[test]
	fn a_store_into_one_code_page_keeps_the_other_pages_blocks() {
		let mut cpu = create_cpu();
		cpu.get_mut_mmu().init_memory(65536);
		let (a, b) = (DRAM_BASE, DRAM_BASE + 0x1000);
		for (at, w) in [(a, 0x0012_8293u32), (a + 4, 0x0012_8293), (a + 8, 0x7f90_006f), // addi t0,t0,1 x2; j B
		                (b, 0x0013_0313), (b + 4, 0x0013_0313), (b + 8, 0xff9f_e06f), // addi t1,t1,1 x2; j A
		                (b + 0x800, 0)] {
			cpu.get_mut_mmu().store_word(at, w).ok().unwrap();
		}
		cpu.pc = a;
		cpu.run(3000);
		assert!(cpu.x[5] > 100 && cpu.x[6] > 100, "the loop ran ({} {})", cpu.x[5], cpu.x[6]);
		let slot_of = |pc: u64| ((pc >> 1) as usize) & (BLOCK_SLOTS - 1);
		let (ha, hb) = (cpu.block_heads[slot_of(a)], cpu.block_heads[slot_of(b)]);
		assert_eq!((ha.tag, hb.tag), (a, b), "both blocks cached");
		assert!(cpu.head_valid(&ha) && cpu.head_valid(&hb));
		// data store into code page B (not into its instructions): the epoch and B's generation move
		let epoch = cpu.mmu.code_gen();
		cpu.get_mut_mmu().store_word(b + 0x800, 7).ok().unwrap();
		assert_ne!(cpu.mmu.code_gen(), epoch, "a store into a marked page moves the epoch");
		assert!(cpu.head_valid(&cpu.block_heads[slot_of(a)]), "page A's block survives a store into page B");
		assert!(!cpu.head_valid(&cpu.block_heads[slot_of(b)]), "page B's block is retired");
		// the dispatcher re-stamps A instead of rebuilding it, and the program still runs correctly
		let (x5, x6) = (cpu.x[5], cpu.x[6]);
		cpu.run(3000);
		assert!(cpu.x[5] > x5 && cpu.x[6] > x6, "still loops after the store");
		assert_eq!(cpu.block_heads[slot_of(a)].page_gen, ha.page_gen, "A was kept, not rebuilt");
		// a store into page A retires A's block (code that really changed is never run stale)
		cpu.get_mut_mmu().store_word(a + 0x900, 1).ok().unwrap();
		assert!(!cpu.head_valid(&cpu.block_heads[slot_of(a)]), "page A's block is retired by a store into page A");
		// and a changed instruction is executed as changed: turn A's first addi into addi t0,t0,100
		cpu.get_mut_mmu().store_word(a, 0x0642_8293).ok().unwrap();
		let x5 = cpu.x[5];
		cpu.pc = a;
		cpu.run(40);
		assert!(cpu.x[5] - x5 >= 100, "the rewritten instruction ran ({} -> {})", x5, cpu.x[5]);
	}

	#[test]
	fn double_to_single_preserves_single_bits_and_round_trip() {
		let mut cpu = create_cpu();
		let narrow = INSTRUCTIONS.iter().find(|i| i.name == "FCVT.S.D").unwrap();
		let widen = INSTRUCTIONS.iter().find(|i| i.name == "FCVT.D.S").unwrap();
		for input in [1.0_f64, -1.0, 0.0, -0.0, 1.5, 100.0, f64::INFINITY, f64::NEG_INFINITY, 1e-40] {
			cpu.f[10] = input;
			assert!((narrow.operation)(&mut cpu, 0x4015_05d3, 0).is_ok()); // fcvt.s.d fa1,fa0
			let single = input as f32;
			assert_eq!(cpu.f[11].to_bits(), 0xffff_ffff_0000_0000 | single.to_bits() as u64);
			assert!((widen.operation)(&mut cpu, 0x4205_8653, 0).is_ok()); // fcvt.d.s fa2,fa1
			assert_eq!(cpu.f[12].to_bits(), (single as f64).to_bits());
		}
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

// ---- codegen JIT self-test / benchmark ------------------------------------

/// risc-box patch (codegen): the live JIT against the interpreter on a
/// generated guest, inside whatever process runs it — in the SET component
/// that means the real enclave:codegen verb over the real shared memory64.
///
/// The guest (S-mode, SV39, 32 MiB of copy-on-write RAM shared at start)
/// loops over kernels shaped like the work a desktop does: register ALU
/// loops, strided memory walks (loads, stores, misaligned and page-crossing
/// accesses, first stores into shared chunks), calls and returns, a
/// bytecode interpreter's jump-table dispatch (indirect jumps), FP loops,
/// self-modifying code (it rewrites one of its own functions every
/// iteration), M-extension ops the translator does not cover, and a
/// delegated page fault the trap handler skips. Machine A interprets,
/// machine B runs with the JIT; both run until the guest parks on its final
/// spin loop, then registers, pc, privilege and a hash of all RAM must
/// match. Returns (report, ok); ok also requires that B actually ran
/// compiled code.
#[cfg(feature = "codegen")]
pub fn jit_selftest(seed: u64, steps: u64) -> (String, bool) {
	use std::time::Instant;
	let mut out = String::new();
	// calibrate: instructions per outer iteration, interpreted
	let per_iter = {
		let (mut m, spin) = selftest::machine(seed, 64);
		let (n, _) = selftest::run_to_spin(&mut m, spin, 1 << 30);
		(n / 64).max(1)
	};
	let iters = (steps / per_iter).max(4);
	out.push_str(&format!(
		"jit selftest: seed {} — {} outer iterations x ~{} instructions\n",
		seed, iters, per_iter
	));

	let (mut a, spin) = selftest::machine(seed, iters);
	let t = Instant::now();
	let (na, done_a) = selftest::run_to_spin(&mut a, spin, steps * 4);
	let ta = t.elapsed().as_secs_f64();

	let (mut b, _) = selftest::machine(seed, iters);
	let mut params = JitParams::default();
	params.form_interval = 5_000_000;
	let on = b.jit_enable(params);
	let t = Instant::now();
	// in slices, to see the steady state after compilation settles
	let mut nb = 0u64;
	let mut marks: Vec<(u64, f64)> = Vec::new();
	let mut done_b = false;
	while nb < steps * 4 {
		let (n, d) = selftest::run_to_spin(&mut b, spin, 10_000_000);
		nb += n;
		marks.push((nb, t.elapsed().as_secs_f64()));
		if d {
			done_b = true;
			break;
		}
	}
	let tb = t.elapsed().as_secs_f64();

	let ha = selftest::state_hash(&a);
	let hb = selftest::state_hash(&b);
	let same = done_a && done_b && ha == hb && a.x == b.x && a.pc == b.pc
		&& (0..32).all(|i| a.f[i].to_bits() == b.f[i].to_bits());
	let mips = |n: u64, s: f64| n as f64 / 1e6 / s.max(1e-9);
	// steady state: the second half of B's run
	let half = marks.iter().find(|m| m.0 >= nb / 2).copied().unwrap_or((0, 0.0));
	let steady = mips(nb - half.0, tb - half.1);
	let js = b.jit_stats().unwrap_or_default();
	let vs = ::jit::verb::stats();
	out.push_str(&format!(
		"interpreter: {} instructions in {:.3} s = {:.1} MIPS (done={})\n",
		na, ta, mips(na, ta), done_a
	));
	out.push_str(&format!(
		"jit:         {} instructions in {:.3} s = {:.1} MIPS overall, {:.1} MIPS second half (done={}, enabled={})\n",
		nb, tb, mips(nb, tb), steady, done_b, on
	));
	out.push_str(&format!(
		"jit coverage: {:.1}% of retired ran compiled ({} calls, {} empty; {} regions live, {} installs, {} formed, {} passes)\n",
		100.0 * js.retired as f64 / nb.max(1) as f64, js.calls, js.empty_calls,
		js.live_regions, js.installs, js.formed, js.passes
	));
	out.push_str(&format!(
		"jit proofs: {} content, {} mapping, {} failed; verb: {} compiled, {} failed, {} bytes, {} reused, {} refused (heat) {} refused (budget), {:.1} ms compiling (max {:.1}), last status {}, disabled {:?}\n",
		js.content_checks, js.map_checks, js.verify_failures, vs.compiled, vs.failed, vs.bytes,
		vs.reused, vs.refused_heat, vs.refused_budget, vs.compile_us as f64 / 1000.0,
		vs.max_compile_us as f64 / 1000.0, vs.last_status, vs.disabled
	));
	out.push_str(&format!(
		"state: interpreter {:016x}, jit {:016x} -> {}\n",
		ha, hb, if same { "IDENTICAL" } else { "MISMATCH" }
	));
	if !same {
		for i in 0..32 {
			if a.x[i] != b.x[i] {
				out.push_str(&format!("  x{} {:#x} vs {:#x}\n", i, a.x[i], b.x[i]));
			}
		}
		out.push_str(&format!("  pc {:#x} vs {:#x}\n", a.pc, b.pc));
	}
	let ok = same && js.retired > 0;
	out.push_str(if ok { "PASS" } else { "FAIL" });
	(out, ok)
}

#[cfg(feature = "codegen")]
mod selftest {
	use super::*;
	use mmu::DRAM_BASE;
	use terminal::DummyTerminal;

	pub const RAM: u64 = 32 << 20;
	const PT_POOL: u64 = DRAM_BASE + 0x1_0000; // page tables, bump-allocated
	const CODE_PA: u64 = DRAM_BASE + 0x10_0000;
	const CODE_VA: u64 = 0x10_0000_0000;
	const DATA_PA: u64 = DRAM_BASE + 0x40_0000;
	const DATA_VA: u64 = 0x20_0000_0000;
	const DATA_LEN: u64 = 8 << 20;
	const UNMAPPED_VA: u64 = 0x30_0000_0000;
	// data layout (offsets from DATA_VA)
	const ARRAY: u64 = 0x1_0000; // memory kernel walks 0x1_0000..0x9_0000
	const FPA: u64 = 0x10_0000; // FP kernel array
	const BYTECODE: u64 = 0x20_0000;
	const TABLE: u64 = 0x20_8000;
	const RESULT: u64 = 0x30_0000;

	struct Rng(u64);
	impl Rng {
		fn next(&mut self) -> u64 {
			self.0 ^= self.0 << 13;
			self.0 ^= self.0 >> 7;
			self.0 ^= self.0 << 17;
			self.0
		}
	}

	// registers
	const ZERO: u32 = 0;
	const RA: u32 = 1;
	const SP: u32 = 2;
	const T0: u32 = 5;
	const T1: u32 = 6;
	const T2: u32 = 7;
	const S0: u32 = 8; // DATA_VA
	const S1: u32 = 9; // outer iterations left
	const A0: u32 = 10;
	const A1: u32 = 11;
	const A2: u32 = 12;
	const S3: u32 = 19; // TABLE
	const S4: u32 = 20; // BYTECODE
	const S5: u32 = 21; // smc function
	const S6: u32 = 22; // unmapped
	const S7: u32 = 23; // FP array
	const S8: u32 = 24; // DATA_VA + ARRAY
	const S9: u32 = 25; // memory walk offset, kept across iterations
	const S11: u32 = 27; // checksum
	const T3: u32 = 28;
	const T4: u32 = 29;
	const T5: u32 = 30;
	const T6: u32 = 31;
	// registers the random ALU bodies may write
	const SCRATCH: [u32; 11] = [5, 6, 7, 28, 29, 30, 31, 13, 14, 15, 16];

	fn r(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
		f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
	}
	fn i(imm: i32, rs1: u32, f3: u32, rd: u32, op: u32) -> u32 {
		((imm as u32) & 0xfff) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op
	}
	fn s(imm: i32, rs2: u32, rs1: u32, f3: u32, op: u32) -> u32 {
		let u = imm as u32;
		((u >> 5) & 0x7f) << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | (u & 0x1f) << 7 | op
	}

	/// A tiny assembler: 4-byte instructions, labels, branch/jump fixups.
	struct Asm {
		code: Vec<u32>,
		labels: std::collections::HashMap<&'static str, usize>,
		fix: Vec<(usize, &'static str, bool)>, // (at, label, is_jal)
	}

	impl Asm {
		fn here(&self) -> usize {
			self.code.len()
		}
		fn va(&self, at: usize) -> u64 {
			CODE_VA + at as u64 * 4
		}
		fn label(&mut self, l: &'static str) {
			self.labels.insert(l, self.code.len());
		}
		fn w(&mut self, word: u32) {
			self.code.push(word);
		}
		fn op(&mut self, f7: u32, f3: u32, rd: u32, rs1: u32, rs2: u32) {
			self.w(r(f7, rs2, rs1, f3, rd, 0x33));
		}
		fn opw(&mut self, f7: u32, f3: u32, rd: u32, rs1: u32, rs2: u32) {
			self.w(r(f7, rs2, rs1, f3, rd, 0x3b));
		}
		fn addi(&mut self, rd: u32, rs1: u32, imm: i32) {
			self.w(i(imm, rs1, 0, rd, 0x13));
		}
		fn li(&mut self, rd: u32, v: i32) {
			// v fits 32 bits: lui + addi
			let lo = (v << 20) >> 20;
			let hi = v.wrapping_sub(lo) as u32;
			if hi != 0 {
				self.w((hi & 0xfffff000) | rd << 7 | 0x37);
				self.addi(rd, rd, lo);
			} else {
				self.addi(rd, ZERO, lo);
			}
		}
		fn ld(&mut self, f3: u32, rd: u32, rs1: u32, imm: i32) {
			self.w(i(imm, rs1, f3, rd, 0x03));
		}
		fn st(&mut self, f3: u32, rs2: u32, rs1: u32, imm: i32) {
			self.w(s(imm, rs2, rs1, f3, 0x23));
		}
		fn br(&mut self, f3: u32, rs1: u32, rs2: u32, l: &'static str) {
			self.fix.push((self.code.len(), l, false));
			self.w(r(0, rs2, rs1, f3, 0, 0x63));
		}
		fn jal(&mut self, rd: u32, l: &'static str) {
			self.fix.push((self.code.len(), l, true));
			self.w(rd << 7 | 0x6f);
		}
		fn jalr(&mut self, rd: u32, rs1: u32, imm: i32) {
			self.w(i(imm, rs1, 0, rd, 0x67));
		}
		fn ret(&mut self) {
			self.jalr(ZERO, RA, 0);
		}
		fn finish(mut self) -> (Vec<u32>, std::collections::HashMap<&'static str, usize>) {
			for &(at, l, is_jal) in &self.fix {
				let off = (self.labels[l] as i64 - at as i64) * 4;
				let u = off as u32;
				let w = self.code[at];
				self.code[at] = match is_jal {
					true => w | ((u >> 20) & 1) << 31 | ((u >> 1) & 0x3ff) << 21
						| ((u >> 11) & 1) << 20 | ((u >> 12) & 0xff) << 12,
					false => w | ((u >> 12) & 1) << 31 | ((u >> 5) & 0x3f) << 25
						| ((u >> 1) & 0xf) << 8 | ((u >> 11) & 1) << 7,
				};
			}
			(self.code, self.labels)
		}
	}

	/// A random straight-line ALU body over the scratch registers.
	fn alu_body(a: &mut Asm, rng: &mut Rng, n: usize) {
		for _ in 0..n {
			let pick = |rng: &mut Rng| SCRATCH[(rng.next() % SCRATCH.len() as u64) as usize];
			let (rd, x, y) = (pick(rng), pick(rng), pick(rng));
			let imm = (rng.next() % 4096) as i32 - 2048;
			match rng.next() % 16 {
				0 => a.op(0, 0, rd, x, y),          // add
				1 => a.op(0x20, 0, rd, x, y),       // sub
				2 => a.op(0, 4, rd, x, y),          // xor
				3 => a.op(0, 6, rd, x, y),          // or
				4 => a.op(0, 7, rd, x, y),          // and
				5 => a.op(1, 0, rd, x, y),          // mul
				6 => a.op(0, 1, rd, x, y),          // sll
				7 => a.op(0x20, 5, rd, x, y),       // sra
				8 => a.op(0, 3, rd, x, y),          // sltu
				9 => a.opw(0, 0, rd, x, y),         // addw
				10 => a.opw(0x20, 0, rd, x, y),     // subw
				11 => a.addi(rd, x, imm),
				12 => a.w(i(imm, x, 4, rd, 0x13)),  // xori
				13 => a.w(i((rng.next() % 64) as i32, x, 5, rd, 0x13)), // srli
				14 => a.w(i(imm, x, 0, rd, 0x1b)),  // addiw
				_ => a.w(i(0x400 | (rng.next() % 32) as i32, x, 5, rd, 0x1b)), // sraiw
			}
		}
	}

	/// The guest program; returns (code words, labels).
	fn program(seed: u64, iters: u64) -> (Vec<u32>, std::collections::HashMap<&'static str, usize>) {
		let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
		let mut a = Asm { code: Vec::new(), labels: Default::default(), fix: Vec::new() };
		// main
		a.li(S1, iters as i32);
		a.label("outer");
		a.li(A0, 40);
		a.jal(RA, "k_alu");
		a.li(A0, 96);
		a.addi(A1, S9, 0);
		a.jal(RA, "k_mem");
		a.addi(S9, A1, 0);
		a.li(A0, 24);
		a.jal(RA, "k_call");
		a.addi(A1, S4, 0);
		a.jal(RA, "k_interp");
		a.li(A0, 32);
		a.addi(A1, S7, 0);
		a.jal(RA, "k_fp");
		// self-modifying code, every 16th iteration: smc_f (on its own page)
		// gets a new first word, addi a0, a0, (s1 & 0x7ff)
		a.w(i(15, S1, 7, T0, 0x13)); // andi t0, s1, 15
		a.br(1, T0, ZERO, "no_smc");
		a.w(i(0x7ff, S1, 7, T0, 0x13)); // andi t0, s1, 0x7ff
		a.w(i(20, T0, 1, T0, 0x13)); // slli t0, t0, 20
		a.li(T1, i(0, A0, 0, A0, 0x13) as i32); // addi a0, a0, 0
		a.op(0, 6, T0, T0, T1); // or
		a.st(2, T0, S5, 0); // sw t0, 0(s5)
		a.label("no_smc");
		a.addi(A0, S1, 0);
		a.jalr(RA, S5, 0);
		a.op(0, 0, S11, S11, A0);
		a.li(A0, 12);
		a.jal(RA, "k_mext");
		a.ld(3, T0, S6, 0); // faults: the handler skips it
		a.addi(S1, S1, -1);
		a.br(1, S1, ZERO, "outer");
		a.li(T0, RESULT as i32);
		a.op(0, 0, T0, S0, T0);
		a.st(3, S11, T0, 0);
		a.label("spin");
		a.jal(ZERO, "spin");

		// k_alu(a0): a random ALU loop folded into the checksum
		a.label("k_alu");
		a.label("k_alu_loop");
		alu_body(&mut a, &mut rng, 14);
		a.op(0, 0, S11, S11, T0);
		a.op(0, 4, S11, S11, T3);
		a.addi(A0, A0, -1);
		a.br(1, A0, ZERO, "k_alu_loop");
		a.ret();

		// k_mem(a0 count, a1 offset): strided walk over a 512 KiB window
		// (S8) with stores, misaligned and page-crossing accesses; the odd
		// stride walks the offsets through every alignment
		a.label("k_mem");
		a.label("k_mem_loop");
		a.op(0, 0, T6, S8, A1);
		a.ld(3, T0, T6, 0);
		a.ld(3, T1, T6, 8);
		a.op(0, 0, T0, T0, T1);
		a.op(0, 4, T0, T0, S11);
		a.st(3, T0, T6, 16);
		a.ld(2, T2, T6, 4); // lw
		a.st(2, T2, T6, 24); // sw
		a.ld(4, T3, T6, 3); // lbu
		a.st(0, T3, T6, 31); // sb
		a.ld(5, T4, T6, 6); // lhu
		a.st(1, T4, T6, 38); // sh
		a.ld(3, T5, T6, 43); // ld, any alignment
		a.op(0, 0, S11, S11, T5);
		a.op(0, 0, S11, S11, T0);
		a.li(T5, 4093);
		a.op(0, 0, A1, A1, T5);
		a.li(T5, 0x7ffff);
		a.op(0, 7, A1, A1, T5);
		a.addi(A0, A0, -1);
		a.br(1, A0, ZERO, "k_mem_loop");
		a.ret();

		// k_call(a0): calls through JAL and through a function pointer
		a.label("k_call");
		a.addi(SP, SP, -16);
		a.st(3, RA, SP, 0);
		a.label("k_call_loop");
		a.addi(T0, A0, 0);
		a.jal(RA, "helper");
		a.op(0, 0, S11, S11, T0);
		a.ld(3, T1, S3, 64); // a function pointer: table[8] = helper2
		a.jalr(RA, T1, 0);
		a.op(0, 0, S11, S11, T0);
		a.addi(A0, A0, -1);
		a.br(1, A0, ZERO, "k_call_loop");
		a.ld(3, RA, SP, 0);
		a.addi(SP, SP, 16);
		a.ret();
		a.label("helper2");
		a.op(1, 0, T0, T0, T0);
		a.addi(T0, T0, 7);
		a.ret();
		a.label("helper");
		a.op(0, 0, T2, T0, T0);
		a.op(0, 0, T0, T2, T0);
		a.addi(T0, T0, 1);
		a.ret();

		// k_interp(a1 = bytecode): jump-table dispatch, LLInt-shaped
		a.label("k_interp");
		a.label("dispatch");
		a.ld(4, T0, A1, 0); // lbu op
		a.addi(A1, A1, 1);
		a.w(i(3, T0, 1, T0, 0x13)); // slli t0, t0, 3
		a.op(0, 0, T0, T0, S3);
		a.ld(3, T0, T0, 0);
		a.jalr(ZERO, T0, 0);
		for h in 0..7 {
			let name: &'static str = ["h0", "h1", "h2", "h3", "h4", "h5", "h6"][h];
			a.label(name);
			alu_body(&mut a, &mut rng, 2 + h);
			a.op(0, 0, S11, S11, T3);
			a.jal(ZERO, "dispatch");
		}
		a.label("h7");
		a.ret();

		// k_fp(a0, a1): FP multiply-accumulate
		a.label("k_fp");
		a.label("k_fp_loop");
		a.w(i(0, A1, 3, 0, 0x07)); // fld f0, 0(a1)
		a.w(i(8, A1, 3, 1, 0x07)); // fld f1, 8(a1)
		a.w(r(0x09, 1, 0, 0, 2, 0x53)); // fmul.d f2, f0, f1
		a.w(r(0x01, 2, 3, 0, 3, 0x53)); // fadd.d f3, f3, f2
		a.w(r(0x05, 1, 3, 0, 4, 0x53)); // fsub.d f4, f3, f1
		a.w(s(16, 4, A1, 3, 0x27)); // fsd f4, 16(a1)
		a.w(r(0x71, 0, 3, 0, T0, 0x53)); // fmv.x.d t0, f3
		a.op(0, 4, S11, S11, T0);
		a.w(r(0x69, 0, A0, 0, 5, 0x53)); // fcvt.d.w f5, a0
		a.w(r(0x01, 5, 3, 0, 3, 0x53)); // fadd.d f3, f3, f5
		a.addi(A1, A1, 24);
		a.addi(A0, A0, -1);
		a.br(1, A0, ZERO, "k_fp_loop");
		a.ret();

		// k_mext(a0): M-extension ops the translator leaves to the interpreter
		a.label("k_mext");
		a.label("k_mext_loop");
		a.op(1, 4, T0, S11, A0); // div
		a.op(1, 6, T1, S11, A0); // rem
		a.op(1, 3, T2, S11, A0); // mulhu
		a.opw(1, 5, T3, S11, A0); // divuw
		a.op(0, 0, S11, S11, T0);
		a.op(0, 4, S11, S11, T1);
		a.op(0, 0, S11, S11, T2);
		a.op(0, 0, S11, S11, T3);
		a.addi(A0, A0, -1);
		a.br(1, A0, ZERO, "k_mext_loop");
		a.ret();

		// trap handler (stvec): skip the faulting instruction
		a.label("trap");
		a.w(i(0x141, 0, 2, T6, 0x73)); // csrr t6, sepc
		a.addi(T6, T6, 4);
		a.w(i(0x141, T6, 1, 0, 0x73)); // csrw sepc, t6
		a.w(0x1020_0073); // sret
		// the self-modified function, alone on the next page
		while a.here() % 1024 != 0 {
			a.w(0x0000_0013); // nop padding
		}
		a.label("smc_f");
		a.addi(A0, A0, 0);
		a.addi(A0, A0, 3);
		a.ret();
		a.finish()
	}

	/// Build the machine: program, page tables, data, registers; then share
	/// its RAM so every chunk starts copy-on-write.
	pub fn machine(seed: u64, iters: u64) -> (Cpu, u64) {
		let mut rng = Rng(seed ^ 0x5eed_5eed_5eed_5eed | 1);
		let mut cpu = Cpu::new(Box::new(DummyTerminal::new()));
		cpu.get_mut_mmu().init_memory(RAM);
		let (code, labels) = program(seed, iters);
		for (k, &w) in code.iter().enumerate() {
			let _ = cpu.mmu.store_word(CODE_PA + k as u64 * 4, w);
		}
		// page tables
		let root = PT_POOL;
		let mut next = PT_POOL + 0x1000;
		let mut map = |cpu: &mut Cpu, va: u64, pa: u64, flags: u64| {
			let vpn = [(va >> 12) & 0x1ff, (va >> 21) & 0x1ff, (va >> 30) & 0x1ff];
			let mut table = root;
			for level in (1..3).rev() {
				let at = table + vpn[level] * 8;
				let pte = cpu.mmu.load_doubleword(at).unwrap_or(0);
				table = match pte & 1 {
					1 => (pte >> 10) << 12,
					_ => {
						let t = next;
						next += 0x1000;
						let _ = cpu.mmu.store_doubleword(at, ((t >> 12) << 10) | 1);
						t
					}
				};
			}
			let _ = cpu.mmu.store_doubleword(table + vpn[0] * 8, ((pa >> 12) << 10) | flags | 0xc1);
		};
		let code_pages = (code.len() as u64 * 4 + 0xfff) / 0x1000;
		for p in 0..code_pages {
			map(&mut cpu, CODE_VA + p * 0x1000, CODE_PA + p * 0x1000, 0x0e); // R W X
		}
		for p in 0..DATA_LEN / 0x1000 {
			map(&mut cpu, DATA_VA + p * 0x1000, DATA_PA + p * 0x1000, 0x06); // R W
		}
		// data: random words in the walk window, finite doubles for FP,
		// a bytecode program ending in op 7, the handler table
		for o in (ARRAY..ARRAY + 0x8_0100).step_by(8) {
			let v = rng.next();
			let _ = cpu.mmu.store_doubleword(DATA_PA + o, v);
		}
		for k in 0..(32 * 3 + 3) as u64 {
			let v = ((rng.next() % 2_000_000) as f64 / 1000.0 - 1000.0).to_bits();
			let _ = cpu.mmu.store_doubleword(DATA_PA + FPA + k * 8, v);
		}
		for k in 0..200u64 {
			let op = if k == 199 { 7 } else { (rng.next() % 7) as u8 };
			cpu.mmu.store_raw(DATA_PA + BYTECODE + k, op);
		}
		for h in 0..9u64 {
			let name = ["h0", "h1", "h2", "h3", "h4", "h5", "h6", "h7", "helper2"][h as usize];
			let _ = cpu.mmu.store_doubleword(DATA_PA + TABLE + h * 8, CODE_VA + labels[name] as u64 * 4);
		}
		// S-mode, SV39, page faults delegated to the S handler, no interrupts
		cpu.write_csr_raw(CSR_MEDELEG_ADDRESS, 0xb000);
		cpu.write_csr_raw(CSR_STVEC_ADDRESS, CODE_VA + labels["trap"] as u64 * 4);
		cpu.write_csr_raw(CSR_MIE_ADDRESS, 0);
		cpu.update_addressing_mode((8 << 60) | (root >> 12));
		cpu.privilege_mode = PrivilegeMode::Supervisor;
		cpu.mmu.update_privilege_mode(PrivilegeMode::Supervisor);
		for k in 1..32 {
			cpu.x[k] = rng.next() as i64;
		}
		cpu.x[SP as usize] = (DATA_VA + 0x38_0000) as i64;
		cpu.x[S0 as usize] = DATA_VA as i64;
		cpu.x[S3 as usize] = (DATA_VA + TABLE) as i64;
		cpu.x[S4 as usize] = (DATA_VA + BYTECODE) as i64;
		cpu.x[S5 as usize] = (CODE_VA + labels["smc_f"] as u64 * 4) as i64;
		cpu.x[S6 as usize] = UNMAPPED_VA as i64;
		cpu.x[S7 as usize] = (DATA_VA + FPA) as i64;
		cpu.x[S8 as usize] = (DATA_VA + ARRAY) as i64;
		cpu.x[S9 as usize] = 0;
		cpu.x[S11 as usize] = 0;
		for k in 0..32 {
			cpu.f[k] = 0.0;
		}
		cpu.update_pc(CODE_VA);
		// every chunk copy-on-write from here: first stores take the slow path
		let _ = cpu.mmu.share_ram();
		let spin = CODE_VA + labels["spin"] as u64 * 4;
		(cpu, spin)
	}

	/// Run until the guest parks on its spin loop or `budget` retires.
	/// Returns (instructions retired, parked).
	pub fn run_to_spin(cpu: &mut Cpu, spin: u64, budget: u64) -> (u64, bool) {
		let start = cpu.retired();
		while cpu.retired() - start < budget {
			cpu.run(100_000);
			if cpu.pc == spin {
				return (cpu.retired() - start, true);
			}
		}
		(cpu.retired() - start, cpu.pc == spin)
	}

	/// FNV over every RAM page plus privilege and the S-mode trap CSRs.
	pub fn state_hash(cpu: &Cpu) -> u64 {
		let mut h: u64 = 0xcbf29ce484222325;
		let mut mix = |b: &[u8]| {
			for &x in b {
				h ^= x as u64;
				h = h.wrapping_mul(0x100000001b3);
			}
		};
		let mut page = vec![0u8; 4096];
		for p in 0..RAM / 4096 {
			cpu.mmu.read_physical_range(DRAM_BASE + p * 4096, &mut page);
			mix(&page);
		}
		for c in [CSR_SEPC_ADDRESS, CSR_SCAUSE_ADDRESS, CSR_STVAL_ADDRESS] {
			mix(&cpu.read_csr_raw(c).to_le_bytes());
		}
		mix(&[get_privilege_encoding(&cpu.privilege_mode)]);
		h
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		/// The generated guest itself, interpreted: it must reach its spin
		/// loop, take its page faults through the handler, rewrite its own
		/// code, and be deterministic.
		#[test]
		fn guest_program_runs_to_completion_deterministically() {
			let (mut a, spin) = machine(3, 18);
			let (n, done) = run_to_spin(&mut a, spin, 1 << 28);
			assert!(done, "parked after {} instructions", n);
			assert!(n > 50_000, "a real workload: {}", n);
			assert_eq!(a.read_csr_raw(CSR_SCAUSE_ADDRESS), 13, "the load page fault was taken");
			let result = a.mmu.load_word_raw(DATA_PA + RESULT) as u64
				| (a.mmu.load_word_raw(DATA_PA + RESULT + 4) as u64) << 32;
			assert_eq!(result as i64, a.x[S11 as usize], "checksum stored");
			// smc_f's first word was rewritten at s1 = 16 (iterations run 18..1)
			let (_, labels) = program(3, 18);
			assert_eq!(labels["smc_f"] % 1024, 0, "own page");
			let w = a.mmu.load_word_raw(CODE_PA + labels["smc_f"] as u64 * 4);
			assert_eq!(w, i(16, A0, 0, A0, 0x13), "self-modified");
			let (mut b, _) = machine(3, 18);
			let (m, _) = run_to_spin(&mut b, spin, 1 << 28);
			assert_eq!((state_hash(&a), a.x, a.pc), (state_hash(&b), b.x, b.pc));
			assert_eq!(n, m);
		}

		/// risc-box patch (packed modules): stand-ins for the verb's compile
		/// and call (natively there is neither) and the formation tests'
		/// guest, heat and placement helpers.
		mod packs {
			use std::sync::atomic::{AtomicU64, Ordering};
			use std::sync::Mutex;
			use super::*;
			pub static MODULES: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
			/// (table index, entry, bias in the context block) per call
			pub static CALLS: Mutex<Vec<(u64, u32, u64)>> = Mutex::new(Vec::new());
			/// pcs the stand-in call "exits" at, in order (none left: it runs nothing)
			pub static EXITS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
			pub static PC_OFF: AtomicU64 = AtomicU64::new(0);
			/// stand-in compiler: keeps every module, hands out table indices 7000, 7001, ...
			pub fn compile(m: &[u8]) -> i64 {
				let mut v = MODULES.lock().unwrap();
				v.push(m.to_vec());
				7000 + v.len() as i64 - 1
			}
			/// stand-in call: logs it, and "exits" at the next EXITS pc after 5 instructions (or runs nothing)
			pub fn call(index: u64, _fuel: u64, entry: u32) -> u64 {
				let ctx = &::jit::verb::CTX;
				CALLS.lock().unwrap().push((index, entry, ctx.get(::jit::CTX_BIAS)));
				let mut exits = EXITS.lock().unwrap();
				if exits.is_empty() {
					return 0;
				}
				// what generated code does: pc written through the context block's state base
				let pc = exits.remove(0);
				unsafe { *((ctx.get(::jit::CTX_BASE) + PC_OFF.load(Ordering::Relaxed)) as usize as *mut u64) = pc };
				5
			}
			/// Fresh verb state with the stand-ins (the caller holds TEST_SERIAL).
			pub fn reset(policy: ::jit::verb::Policy) {
				MODULES.lock().unwrap().clear();
				CALLS.lock().unwrap().clear();
				EXITS.lock().unwrap().clear();
				::jit::verb::reset_for_test(Some(compile), policy);
				::jit::verb::set_test_caller(Some(call));
			}
			/// The self-test guest (seed 11), interpreted to its spin loop, and the cached blocks a region can be
			/// formed from: valid now, fetched from the page they were built from, on a page that is not being
			/// rewritten, first op translated. Several sit on each code page.
			pub fn guest() -> (Cpu, Vec<u64>) {
				let (mut cpu, spin) = machine(11, 6);
				assert!(run_to_spin(&mut cpu, spin, 1 << 28).1);
				let cg = cpu.mmu.code_gen();
				let mut pcs = Vec::new();
				for slot in 0..BLOCK_SLOTS {
					let h = cpu.block_heads[slot];
					if h.tag == 0 || h.count == 0 || !cpu.head_valid(&h) {
						continue;
					}
					match cpu.mmu.translate_fetch_probe(h.tag) {
						Ok(p) if (p & !0xfff) == h.phys_page => {}
						_ => continue,
					}
					if cpu.mmu.page_gen(h.phys_page) >= JIT_REWRITTEN_PAGE_GEN {
						continue;
					}
					match cpu.jit_block_ops(h.tag, cg) {
						Some(ops) if ::jit::translatable(&ops[0]) => pcs.push(h.tag),
						_ => {}
					}
				}
				pcs.sort();
				assert!(pcs.len() >= 24, "cached blocks: {}", pcs.len());
				(cpu, pcs)
			}
			/// Sampled heat and successions for each (region, its total heat): 100 rounds of a cycle through its
			/// blocks, nothing between regions.
			pub fn heat(cpu: &mut Cpu, regions: &[(Vec<u64>, u64)]) {
				let j = cpu.jit.as_deref_mut().unwrap();
				for (r, h) in regions {
					j.t2.note_break();
					for _ in 0..100 {
						for &pc in r {
							j.t2.note_block(pc, h / 100 / r.len() as u64);
						}
					}
				}
				j.t2.note_break();
			}
			/// Where each region was installed: (instance, entry base, bias, table index), checking that every
			/// member slot points at that instance with entry base + i and that the instance holds exactly the
			/// region's own members. None for a region not installed.
			pub fn placed(cpu: &Cpu, regions: &[Vec<u64>]) -> Vec<Option<(u32, u32, u64, u64)>> {
				let j = cpu.jit.as_deref().unwrap();
				regions.iter().map(|r| {
					let s0 = j.slots[((r[0] >> 1) as usize) & (BLOCK_SLOTS - 1)];
					if s0.tag != r[0] {
						return None;
					}
					let reg = &j.regions[s0.region as usize];
					assert_eq!(reg.bias, r[0] & !0xfff);
					assert_eq!(reg.members.iter().map(|m| m.0).collect::<Vec<u64>>(), *r, "the region's own members");
					for (i, &pc) in r.iter().enumerate() {
						let s = j.slots[((pc >> 1) as usize) & (BLOCK_SLOTS - 1)];
						assert_eq!((s.tag, s.region, s.entry), (pc, s0.region, reg.entry_base + i as u32));
					}
					Some((s0.region, reg.entry_base, reg.bias, reg.index))
				}).collect()
			}
		}

		/// risc-box patch (packed modules), at formation level: eight hot
		/// regions formed in one pass (three cached blocks of the self-test
		/// guest each, several on one code page) are compiled as ONE module,
		/// each installed as its own instance with its own entry range, and
		/// none runs before its own proof: a call that exits at another
		/// group's entry chains into it with THAT group's bias, and a group
		/// whose code changed does not run while its module-mate does. The
		/// same regions formed again — on this machine once its instances are
		/// gone, or on another machine — install from the placement cache
		/// without a compile.
		#[test]
		fn formation_packs_hot_regions_into_one_module() {
			extern crate wasmtime;
			use self::packs::*;
			use std::sync::atomic::Ordering;
			let _l = ::jit::verb::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset(::jit::verb::Policy::default());
			let params = JitParams::default();
			let (mut a, pcs) = guest();
			let regions: Vec<Vec<u64>> = pcs.chunks(3).take(8).map(|c| c.to_vec()).collect();
			let hot: Vec<(Vec<u64>, u64)> = regions.iter().map(|r| (r.clone(), 150_000)).collect();
			assert!(a.jit_enable(params.clone()));
			a.jit_prepare();
			heat(&mut a, &hot);
			a.jit_form_pass();

			// ONE module holds all eight
			let v = ::jit::verb::stats();
			assert_eq!((v.compiled, MODULES.lock().unwrap().len()), (1, 1), "{:?}", v);
			let bytes = MODULES.lock().unwrap()[0].clone();
			let mut c = wasmtime::Config::new();
			c.wasm_memory64(true);
			c.wasm_threads(true);
			wasmtime::Module::validate(&wasmtime::Engine::new(&c).unwrap(), &bytes).expect("the pack is valid wasm");
			// (instance, entry base, bias) per region, all in module 7000
			let placed = |cpu: &Cpu| -> Vec<(u32, u32, u64)> {
				packs::placed(cpu, &regions).into_iter().map(|p| {
					let p = p.expect("every region installed");
					assert_eq!(p.3, 7000);
					(p.0, p.1, p.2)
				}).collect()
			};
			let p = placed(&a);
			let mut bases: Vec<u32> = p.iter().map(|x| x.1).collect();
			bases.sort();
			assert_eq!(bases, (0..8).map(|k| 3 * k).collect::<Vec<u32>>(), "one entry range per region");
			let mut rids: Vec<u32> = p.iter().map(|x| x.0).collect();
			rids.sort();
			rids.dedup();
			assert_eq!(rids.len(), 8, "one instance per region");
			let shared = p.iter().filter(|x| p.iter().filter(|y| y.2 == x.2).count() > 1).count();
			assert!(shared >= 2, "regions sharing a bias (one code page) keep separate instances");
			{
				let j = a.jit.as_deref().unwrap();
				assert_eq!((j.stats.packs, j.stats.packed_regions, j.stats.pack_reused), (1, 8, 0));
				assert_eq!((j.stats.installs, j.stats.live_regions, j.stats.refused), (8, 8, 0));
				eprintln!("pack: {} regions, {} entries, {} bytes", j.stats.packed_regions, 24, bytes.len());
				// installed, not run: nothing is proven yet
				assert!(p.iter().all(|x| !j.regions[x.0 as usize].ok && j.regions[x.0 as usize].checked == (0, 0)));
			}

			// a call into region 0 that exits at region 1's first block chains into region 1 — same module,
			// its own entry and ITS bias — after region 1's own proof; region 2..7 are still unproven
			PC_OFF.store(a.jit.as_deref().unwrap().lay.pc_addr, Ordering::Relaxed);
			*EXITS.lock().unwrap() = vec![regions[1][0], 0x1234];
			a.update_pc(regions[0][0]);
			let ran = a.jit_run(((regions[0][0] >> 1) as usize) & (BLOCK_SLOTS - 1), regions[0][0]);
			assert_eq!(ran, 10);
			assert_eq!(*CALLS.lock().unwrap(), vec![(7000, p[0].1, p[0].2), (7000, p[1].1, p[1].2)]);
			{
				let j = a.jit.as_deref().unwrap();
				assert!(j.regions[p[0].0 as usize].ok && j.regions[p[1].0 as usize].ok);
				assert_eq!((j.stats.content_checks, j.stats.calls), (2, 2), "one proof per region that ran");
				assert!(j.stats.chained >= 1);
				assert!(p[2..].iter().all(|x| j.regions[x.0 as usize].checked == (0, 0)));
			}

			// the same regions formed again after this machine's instances are gone (a RAM resize drops them):
			// installed from the placement cache, nothing compiled
			a.jit.as_deref_mut().unwrap().clear();
			heat(&mut a, &hot);
			a.jit_form_pass();
			assert_eq!(::jit::verb::stats().compiled, 1, "re-formed regions compile nothing");
			let p = placed(&a);
			{
				let j = a.jit.as_deref().unwrap();
				assert_eq!((j.stats.packs, j.stats.pack_reused, j.stats.live_regions), (1, 8, 8));
			}

			// ... and on another machine running the same code
			let (mut b, pcs_b) = guest();
			assert_eq!(pcs_b, pcs);
			assert!(b.jit_enable(params.clone()));
			b.jit_prepare();
			heat(&mut b, &hot);
			b.jit_form_pass();
			assert_eq!(::jit::verb::stats().compiled, 1, "another machine compiles nothing either");
			let pb = placed(&b);
			assert_eq!(pb.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>(), p.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>());
			assert_eq!(b.jit.as_deref().unwrap().stats.pack_reused, 8);

			// region 2's code changes: it no longer runs, while region 3 — same module — still does
			let pc2 = regions[2][0];
			let phys = a.mmu.translate_fetch_probe(pc2).unwrap();
			let old = a.mmu.load_word_raw(phys);
			let new: u32 = if old == 0x0010_0013 { 0x0020_0013 } else { 0x0010_0013 }; // addi x0, x0, 1|2
			for (k, byte) in new.to_le_bytes().iter().enumerate() {
				a.mmu.store_raw(phys + k as u64, *byte);
			}
			CALLS.lock().unwrap().clear();
			EXITS.lock().unwrap().clear();
			assert_eq!(a.jit_run(((pc2 >> 1) as usize) & (BLOCK_SLOTS - 1), pc2), 0);
			assert!(CALLS.lock().unwrap().is_empty(), "a group whose proof fails is never called");
			let pc3 = regions[3][0];
			assert_eq!(a.jit_run(((pc3 >> 1) as usize) & (BLOCK_SLOTS - 1), pc3), 0);
			assert_eq!(*CALLS.lock().unwrap(), vec![(7000, p[3].1, p[3].2)]);
			{
				let j = a.jit.as_deref().unwrap();
				assert!(!j.regions[p[2].0 as usize].ok && j.regions[p[3].0 as usize].ok);
			}
			::jit::verb::set_test_caller(None);
		}

		/// The packer under the size cap and the per-pass compile limit:
		/// regions go in hottest first, every module stays under
		/// max_module_bytes (set so that any region fits alone and any two
		/// fit together) and holds several regions, a pass compiles at most
		/// max_compiles_per_pass modules, and the regions left over are packed
		/// by later passes while the ones already placed re-install from the
		/// placement cache.
		#[test]
		fn formation_fills_modules_to_the_cap_hottest_first() {
			use self::packs::*;
			let _l = ::jit::verb::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset(::jit::verb::Policy::default());
			let (mut a, pcs) = guest();
			let regions: Vec<Vec<u64>> = pcs.chunks(3).take(8).map(|c| c.to_vec()).collect();
			// region k is hotter than region k + 1; the coldest still clears its need by itself
			let hot: Vec<(Vec<u64>, u64)> =
				regions.iter().enumerate().map(|(k, r)| (r.clone(), (9 - k as u64) * 10_000)).collect();
			let mut params = JitParams::default();
			params.max_compiles_per_pass = 1;
			assert!(a.jit_enable(params.clone()));
			// each region's group, emitted as the packer will
			let cg = a.mmu.code_gen();
			let costs: Vec<usize> = regions.iter().map(|r| {
				let bias = r[0] & !0xfff;
				let rel: Vec<(u64, Vec<BlockOp>)> = r.iter().map(|&pc| (pc - bias, a.jit_block_ops(pc, cg).unwrap())).collect();
				::jit::emit_group(&rel, &a.jit.as_deref().unwrap().lay).unwrap().pack_cost()
			}).collect();
			let cap = ::jit::PACK_FIXED_BOUND + 2 * costs.iter().max().unwrap() + 16;
			a.jit.as_deref_mut().unwrap().params.max_module_bytes = cap;
			a.jit_prepare();
			let mut passes = 0;
			let mut first_pass = Vec::new();
			while packs::placed(&a, &regions).iter().any(|p| p.is_none()) {
				passes += 1;
				assert!(passes <= 4, "every region placed within a few passes");
				let before = ::jit::verb::stats().compiled;
				heat(&mut a, &hot);
				a.jit_form_pass();
				let compiled = ::jit::verb::stats().compiled - before;
				assert_eq!(compiled, 1, "one module per pass (max_compiles_per_pass)");
				if passes == 1 {
					first_pass = packs::placed(&a, &regions);
				}
			}
			assert!(passes >= 2, "the first module could not hold every region");
			let mods = MODULES.lock().unwrap().clone();
			assert!(mods.iter().all(|m| m.len() <= cap), "cap {}: {:?}", cap, mods.iter().map(|m| m.len()).collect::<Vec<_>>());
			// hottest first: the hottest region is in the first module
			assert_eq!(first_pass[0].map(|p| p.3), Some(7000), "{:?}", first_pass);
			let p: Vec<(u32, u32, u64, u64)> = packs::placed(&a, &regions).into_iter().map(|p| p.unwrap()).collect();
			let mut per_module = std::collections::BTreeMap::new();
			for x in p.iter() {
				*per_module.entry(x.3).or_insert(0) += 1;
			}
			assert_eq!(per_module.len(), mods.len());
			assert!(mods.len() <= 4, "two regions or more per module: {:?}", per_module);
			let j = a.jit.as_deref().unwrap();
			assert_eq!((j.stats.packs as usize, j.stats.packed_regions), (mods.len(), 8));
			let early = first_pass.iter().filter(|p| p.is_some()).count() as u64;
			assert!(j.stats.pack_reused >= early * (passes - 1),
				"placed regions re-formed in later passes re-install without a compile");
			eprintln!("cap {}: group costs {:?}; {} modules {:?} bytes, regions per module {:?}, {} passes",
				cap, costs, mods.len(), mods.iter().map(|m| m.len()).collect::<Vec<_>>(),
				per_module.values().collect::<Vec<_>>(), passes);
		}

		/// Once the module budget is spent the regions still waiting are
		/// counted as refused for budget — one count per REGION, as when
		/// every region had its own lookup — and nothing more is emitted.
		#[test]
		fn pack_budget_refusals_count_regions() {
			use self::packs::*;
			let _l = ::jit::verb::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			reset(::jit::verb::Policy { module_budget: 1, ..::jit::verb::Policy::default() });
			let (mut a, pcs) = guest();
			let regions: Vec<Vec<u64>> = pcs.chunks(3).take(8).map(|c| c.to_vec()).collect();
			let hot: Vec<(Vec<u64>, u64)> =
				regions.iter().enumerate().map(|(k, r)| (r.clone(), (9 - k as u64) * 10_000)).collect();
			let mut params = JitParams::default();
			params.max_module_bytes = 4096; // not all eight in one module
			assert!(a.jit_enable(params));
			a.jit_prepare();
			heat(&mut a, &hot);
			a.jit_form_pass();
			let v = ::jit::verb::stats();
			let j = a.jit.as_deref().unwrap();
			let placed = packs::placed(&a, &regions).iter().filter(|p| p.is_some()).count() as u64;
			assert_eq!((v.compiled, j.stats.packs, j.stats.packed_regions), (1, 1, placed));
			assert!(placed >= 2 && placed < 8, "{}", placed);
			// every region was formed and considered (each fits a module alone): the ones not placed were refused,
			// for budget
			assert_eq!((j.stats.formed, j.stats.oversize, j.stats.refused + placed), (8, 0, 8));
			assert_eq!((v.refused_budget, v.refused_heat), (8 - placed, 0), "{:?} {:?}", v, j.stats);
		}

		/// Admission is per MODULE: a region below the escalated bar rides
		/// along with a hotter one when the module's total heat clears its
		/// total need times the escalation; alone it is refused (and counted
		/// as refused for heat); a region below its OWN unescalated need never
		/// rides along.
		#[test]
		fn pack_admission_is_per_module_with_a_per_region_floor() {
			use self::packs::*;
			let _l = ::jit::verb::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			// the bar doubles with every module compiled
			reset(::jit::verb::Policy { heat_doubling: 1, ..::jit::verb::Policy::default() });
			let (mut a, pcs) = guest();
			let r: Vec<Vec<u64>> = pcs.chunks(3).take(5).map(|c| c.to_vec()).collect();
			let mut params = JitParams::default();
			params.compile_heat_per_op = 0; // need = compile_heat = 6000 for every region
			params.seed_heat = 100;
			params.prune_heat = 1;
			assert!(a.jit_enable(params));
			a.jit_prepare();
			let need = 6000u64;
			// pass 1, bar x1: r0 alone
			heat(&mut a, &[(r[0].clone(), 2 * need)]);
			a.jit_form_pass();
			assert_eq!(::jit::verb::stats().compiled, 1);
			// pass 2, bar x2: r1 (1.5 x need) cannot clear it alone, but with r2 (5 x need) the module holds
			// 6.5 x need >= 2 x 2 x need; r3 (0.5 x need) would still fit the module's bar, but is below its own need
			let refused_heat = ::jit::verb::stats().refused_heat;
			heat(&mut a, &[(r[1].clone(), 3 * need / 2), (r[2].clone(), 5 * need), (r[3].clone(), need / 2)]);
			a.jit_form_pass();
			assert_eq!(::jit::verb::stats().compiled, 2);
			let p = packs::placed(&a, &r);
			assert!(p[1].is_some() && p[2].is_some() && p[1].unwrap().3 == p[2].unwrap().3, "r1 rode along with r2: {:?}", p);
			assert!(p[3].is_none(), "r3 is below its own need");
			assert_eq!(::jit::verb::stats().refused_heat, refused_heat + 1);
			// pass 3, bar x4: r4 (1.5 x need) alone is refused for heat, nothing compiles
			heat(&mut a, &[(r[4].clone(), 3 * need / 2)]);
			a.jit_form_pass();
			assert_eq!(::jit::verb::stats().compiled, 2);
			assert!(packs::placed(&a, &r)[4].is_none());
			assert_eq!(::jit::verb::stats().refused_heat, refused_heat + 2);
		}

		/// Natively there is no verb: the JIT reports itself unavailable and
		/// the self-test fails closed rather than passing on zero coverage.
		#[test]
		fn selftest_fails_closed_without_the_verb() {
			let _l = ::jit::verb::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
			::jit::verb::reset_for_test(None, ::jit::verb::Policy::default());
			let (report, ok) = super::super::jit_selftest(5, 2_000_000);
			assert!(!ok, "{}", report);
			assert!(report.contains("IDENTICAL"), "{}", report);
		}
	}
}

// risc-box patch (jit feature): equivalence between the translator
// (src/jit.rs) and the REAL interpreter (exec_block / exec_op), on
// randomized op sequences over the supported subset — flat memory32 for the
// translator's op semantics, and the production shape (memory64 addresses
// above 4 GiB, chunked copy-on-write RAM, SV39 paging through the machine's
// real software TLB, a nonzero pc bias, in-region indirect jumps) for the
// codegen JIT. This lives here because it compares private machine state.
#[cfg(all(test, feature = "jit"))]
mod test_jit_equivalence {
	extern crate wasmtime;
	use super::*;
	use jit;
	use mmu::DRAM_BASE;
	use terminal::DummyTerminal;

	// ---- flat memory32 layout (translator op semantics) -----------------
	const XB: u64 = 0; // x[32] at 0
	const PCA: u64 = 256;
	const GENA: u64 = 264;
	const FCSRA: u64 = 272; // fcsr (u64)
	const RESF: u64 = 280; // reservation flag (u8)
	const RESA: u64 = 288; // reservation address (u64)
	const FB: u64 = 512; // f[32] as raw 8-byte cells
	const CTXA: u64 = 2048; // context block (base 0, bias 0)
	const DB: u64 = 4096; // linear offset of guest DRAM window
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

	fn engine() -> wasmtime::Engine {
		let mut c = wasmtime::Config::new();
		c.wasm_memory64(true);
		c.wasm_threads(true);
		wasmtime::Engine::new(&c).unwrap()
	}

	/// A test memory and helpers to fill and read it.
	struct Mem {
		store: wasmtime::Store<()>,
		mem: wasmtime::Memory,
	}

	impl Mem {
		fn new(engine: &wasmtime::Engine, memory64: bool, pages: u64) -> Mem {
			let mut store = wasmtime::Store::new(engine, ());
			let ty = match memory64 {
				true => wasmtime::MemoryType::new64(pages, None),
				false => wasmtime::MemoryType::new(pages as u32, None),
			};
			let mem = wasmtime::Memory::new(&mut store, ty).unwrap();
			Mem { store, mem }
		}
		fn put(&mut self, at: u64, bytes: &[u8]) {
			let d = self.mem.data_mut(&mut self.store);
			d[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
		}
		fn put64(&mut self, at: u64, v: u64) {
			self.put(at, &v.to_le_bytes());
		}
		fn get(&self, at: u64, n: usize) -> Vec<u8> {
			self.mem.data(&self.store)[at as usize..at as usize + n].to_vec()
		}
		fn get64(&self, at: u64) -> u64 {
			let mut b = [0u8; 8];
			b.copy_from_slice(&self.get(at, 8));
			u64::from_le_bytes(b)
		}
		fn instance(&mut self, engine: &wasmtime::Engine, bytes: &[u8]) -> wasmtime::Instance {
			let module = wasmtime::Module::new(engine, bytes).expect("valid module");
			let mem = self.mem;
			wasmtime::Instance::new(&mut self.store, &module, &[mem.into()]).expect("instantiates")
		}
		/// emit_block modules: the region shape, run once (fuel 1)
		fn call_block(&mut self, engine: &wasmtime::Engine, bytes: &[u8]) -> u64 {
			self.call_region(engine, bytes, 1, 0)
		}
		fn call_region(&mut self, engine: &wasmtime::Engine, bytes: &[u8], fuel: u64, entry: u32) -> u64 {
			let inst = self.instance(engine, bytes);
			let run = inst.get_typed_func::<(i64, i32), i64>(&mut self.store, "run").unwrap();
			run.call(&mut self.store, (fuel as i64, entry as i32)).unwrap() as u64
		}
		/// call_region for a module compiled once and entered many times
		fn call_module(&mut self, module: &wasmtime::Module, fuel: u64, entry: u32) -> u64 {
			let mem = self.mem;
			let inst = wasmtime::Instance::new(&mut self.store, module, &[mem.into()]).expect("instantiates");
			let run = inst.get_typed_func::<(i64, i32), i64>(&mut self.store, "run").unwrap();
			run.call(&mut self.store, (fuel as i64, entry as i32)).unwrap() as u64
		}
	}

	fn rand_ops(r: &mut Rng, len: usize) -> Vec<BlockOp> {
		let mut ops = Vec::new();
		for _ in 0..len {
			let rd = match (r.next() % 32) as u8 {
				// keep pointer regs (x10..x13) and jump-target regs (x5..x7)
				v @ 10..=13 => v + 10,
				v @ 5..=7 => v + 20,
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
				// FDIV.D shares FMUL.D's slot (picked by a bit already drawn), so
				// the random stream - and the other tests' coverage counts - stay
				// as they were
				48 => (if imm12 & 1 == 0 { HOT_FMUL_D } else { HOT_FDIV_D }, rd, ra, rb, 0, 0),
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
			// mostly finite doubles; one register in six holds a value the
			// float rules single out (SPECIAL_D: zeros, infinities, quiet and
			// signaling NaNs of either sign with payloads, a subnormal, ...).
			// Host and wasm NaN results differ in sign and payload, but both
			// sides canonicalize them now, so NaNs compare bit-exactly too.
			let m = (r.next() % 2000000) as f64 / 1000.0 - 1000.0;
			cpu.f[i] = match r.next() % 6 {
				0 => f64::from_bits(SPECIAL_D[(r.next() % SPECIAL_D.len() as u64) as usize]),
				_ => m,
			};
		}
		for a in (0..WIN).step_by(8) {
			let v = r.next();
			let _ = cpu.get_mut_mmu().store_doubleword(DRAM_BASE + a, v);
		}
		cpu
	}

	fn flat_layout(cpu: &Cpu) -> jit::Layout {
		jit::Layout {
			memory64: false,
			shared: false,
			max_pages: None,
			ctx: CTXA,
			x_base: XB,
			f_base: FB,
			pc_addr: PCA,
			gen_addr: GENA,
			baked_gen: cpu.mmu.code_gen(),
			fcsr_addr: FCSRA,
			res_flag_addr: RESF,
			res_addr_addr: RESA,
			tlb: None,
			guest_dram_base: DRAM_BASE,
			dram_len: WIN,
			ram: jit::Ram::Flat { dram_base: DB },
		}
	}

	/// Serialize the flat state (x, f, pc, gen, DRAM window, context block).
	fn flat_mem(engine: &wasmtime::Engine, cpu_pre: &Cpu, start: u64) -> Mem {
		let mut m = Mem::new(engine, false, 2);
		for i in 0..32 {
			m.put64(XB + i as u64 * 8, cpu_pre.x[i] as u64);
			m.put64(FB + i as u64 * 8, cpu_pre.f[i].to_bits());
		}
		m.put64(PCA, start);
		m.put(GENA, &cpu_pre.mmu.code_gen().to_le_bytes());
		m.put64(FCSRA, cpu_pre.csr[CSR_FCSR_ADDRESS as usize]);
		m.put(RESF, &[cpu_pre.is_reservation_set as u8]);
		m.put64(RESA, cpu_pre.reservation);
		m.put64(CTXA + jit::CTX_BASE, 0);
		m.put64(CTXA + jit::CTX_BIAS, 0);
		let mut win = vec![0u8; WIN as usize];
		cpu_pre.mmu.read_physical_range(DRAM_BASE, &mut win);
		m.put(DB, &win);
		m
	}

	fn flat_state(m: &Mem) -> ([i64; 32], u64, Vec<u8>, [u64; 32]) {
		let mut x = [0i64; 32];
		let mut f = [0u64; 32];
		for i in 0..32 {
			x[i] = m.get64(XB + i as u64 * 8) as i64;
			f[i] = m.get64(FB + i as u64 * 8);
		}
		(x, m.get64(PCA), m.get(DB, WIN as usize), f)
	}

	#[test]
	fn translator_matches_exec_block() {
		let engine = engine();
		let mut checked = 0;
		for seed in 1..400u64 {
			let mut r = Rng(seed * 2654435761 | 1);
			let len = 2 + (r.next() % 12) as usize;
			let ops = rand_ops(&mut r, len);
			let start = DRAM_BASE; // block's pc; DRAM phys tag irrelevant here
			let mut cpu = fresh_cpu(&mut Rng(seed * 40503 | 1));
			let lay = flat_layout(&cpu);
			let bytes = match jit::emit_block(&ops, start, &lay) {
				Some(b) => b,
				None => continue,
			};
			// wasm first (from the pristine state), then the real engine
			let mut m = flat_mem(&engine, &cpu, start);
			let rw = m.call_block(&engine, &bytes);
			let (xw, pcw, dramw, fw) = flat_state(&m);
			let fcsr_w = flat_extra(&m).0;
			cpu.update_pc(start);
			cpu.install_block_for_test(0, start, 0, &ops);
			let ri = cpu.exec_block(0);
			assert_eq!(ri, rw, "retired mismatch seed {}", seed);
			assert_eq!(cpu.csr[CSR_FCSR_ADDRESS as usize], fcsr_w, "fcsr mismatch seed {}", seed);
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

	/// Reference for a region: interpreter-dispatch its blocks (at their
	/// RUNTIME pcs) op by op, exactly as exec_block runs them, until pc
	/// leaves the region, the fuel bound is met at a block entry, or — when
	/// `limit` is given — exactly `limit` ops have retired (the point at
	/// which a compiled region bailed: the op there must NOT run).
	fn region_ref(cpu: &mut Cpu, blocks: &[(u64, Vec<BlockOp>)], fuel: u64, limit: Option<u64>) -> u64 {
		let mut retired = 0u64;
		loop {
			let at = cpu.pc;
			let ops = match blocks.iter().find(|b| b.0 == at) {
				Some(b) => b.1.clone(),
				None => return retired,
			};
			if retired >= fuel {
				return retired;
			}
			let gen = cpu.mmu.code_gen();
			for op in ops.iter() {
				if Some(retired) == limit {
					return retired;
				}
				let address = cpu.pc;
				let next = address.wrapping_add(op.len as u64);
				cpu.pc = next;
				let result = cpu.exec_op(op, address);
				cpu.x[0] = 0;
				retired += 1;
				if let Err(e) = result {
					cpu.handle_exception(e, address);
					return retired;
				}
				if cpu.pc != next {
					break; // taken branch/jump: dispatch again
				}
				if op.kind <= HOT_STORE_MAX && cpu.mmu.code_gen() != gen {
					return retired; // exec_block stops; pc mid-block
				}
			}
		}
	}

	/// two-block counted loop: A does work and loops on itself via BNE,
	/// falls through to B, which stores the result and leaves the region
	fn loop_region(base: u64) -> Vec<(u64, Vec<BlockOp>)> {
		let a = base;
		let b = base + 12;
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
		let engine = engine();
		for &(iters, fuel) in
			&[(1u64, 1u64 << 40), (7, 1 << 40), (1000, 1 << 40), (1000, 7), (1000, 1700), (5, 0)]
		{
			let blocks = loop_region(DRAM_BASE);
			let mut cpu = fresh_cpu(&mut Rng(31337));
			cpu.x[6] = iters as i64;
			cpu.x[10] = (DRAM_BASE + 9000 & !7) as i64;
			let lay = flat_layout(&cpu);
			let bytes = jit::emit_region(&blocks, &lay).expect("region emits");
			let mut m = flat_mem(&engine, &cpu, 0);
			let rw = m.call_region(&engine, &bytes, fuel, 0);
			let (xw, pcw, dramw, _) = flat_state(&m);
			cpu.update_pc(blocks[0].0);
			let ri = region_ref(&mut cpu, &blocks, fuel, None);
			assert_eq!(ri, rw, "retired mismatch iters={} fuel={}", iters, fuel);
			assert_eq!(cpu.pc, pcw, "pc mismatch iters={} fuel={}", iters, fuel);
			assert_eq!(cpu.x, xw, "registers mismatch iters={} fuel={}", iters, fuel);
			let mut dram_i = vec![0u8; WIN as usize];
			cpu.mmu.read_physical_range(DRAM_BASE, &mut dram_i);
			assert_eq!(dram_i, dramw, "dram mismatch iters={} fuel={}", iters, fuel);
		}
	}

	/// jit::translatable must say exactly what emit_seq translates: the
	/// formation trusts it to keep entries that can never run out. Hot kinds
	/// by kind; kind 0 by every INSTRUCTIONS entry.
	#[test]
	fn translatable_matches_the_emitter() {
		let cpu = fresh_cpu(&mut Rng(9));
		let lay = flat_layout(&cpu);
		for kind in 1..=80u8 {
			let o = op(kind, 5, 6, 7, 8);
			let emits = jit::emit_block(&[o], DRAM_BASE, &lay).is_some();
			assert_eq!(jit::translatable(&o), emits, "kind {}", kind);
		}
		let mut names = Vec::new();
		for index in 0..INSTRUCTION_NUM {
			let mut o = op(0, 5, 6, 7, 8);
			o.data = index as u16 | ICACHE_LEN4;
			let emits = jit::emit_block(&[o], DRAM_BASE, &lay).is_some();
			assert_eq!(jit::translatable(&o), emits, "{}", INSTRUCTIONS[index].name);
			if emits {
				names.push(INSTRUCTIONS[index].name);
			}
		}
		// 11 integer/fence ops + MULH x3, 18 AMOs, LR/SC x4, 19 single and
		// 13 double float ops, FCVT.D.S/S.D, FMV.X.W/W.X; the CSR ops only
		// count for fflags/frm/fcsr, and `op` builds a word whose csr is 0
		// (and whose rm is 0, RNE). Not the fused multiply-adds (wasm has no
		// fused multiply-add), FMIN/FMAX or FCLASS.
		assert_eq!(names.len(), 72, "{:?}", names);
		for name in ["FMADD.S", "FMSUB.S", "FNMSUB.S", "FNMADD.S", "FMADD.D", "FMSUB.D", "FNMSUB.D",
			"FNMADD.D", "FMIN.S", "FMAX.S", "FMIN.D", "FMAX.D", "FCLASS.S", "FCLASS.D"]
		{
			assert!(!names.contains(&name), "{}", name);
		}
		// float -> int conversions: a static rm 0-4 (RNE RTZ RDN RUP RMM) is
		// translated; DYN (frm at run time) and the reserved 5/6 are not
		for name in ["FCVT.W.S", "FCVT.WU.S", "FCVT.L.S", "FCVT.LU.S", "FCVT.W.D", "FCVT.WU.D",
			"FCVT.L.D", "FCVT.LU.D"]
		{
			for rm in 0..8u32 {
				let o = decode_op_for_test(&cpu, word_of(name, 5, 6, 0, 0) | rm << 12);
				assert_eq!(op_name(&o), name);
				let emits = jit::emit_block(&[o], DRAM_BASE, &lay).is_some();
				assert_eq!(jit::translatable(&o), emits, "{} rm {}", name, rm);
				assert_eq!(emits, rm <= 4, "{} rm {}", name, rm);
			}
		}
		for (base, name) in [(0x1073u32, "CSRRW"), (0x2073, "CSRRS"), (0x3073, "CSRRC"),
			(0x5073, "CSRRWI"), (0x6073, "CSRRSI"), (0x7073, "CSRRCI")]
		{
			for csr in [1u32, 2, 3, 0x300, 0xc01, 0x180] {
				let o = decode_op_for_test(&cpu, base | 5 << 7 | 6 << 15 | csr << 20);
				assert_eq!(op_name(&o), name);
				let emits = jit::emit_block(&[o], DRAM_BASE, &lay).is_some();
				assert_eq!(jit::translatable(&o), emits, "{} csr {:#x}", name, csr);
				assert_eq!(emits, csr <= 3, "{} csr {:#x}", name, csr);
			}
		}
	}

	/// The M-extension ops and fences the translator takes over from the
	/// table path, against exec_op on edge-heavy operands (zero divisors,
	/// MIN / -1, 32-bit truncation and sign extension).
	#[test]
	fn m_extension_and_fences_match_the_interpreter() {
		let engine = engine();
		let words: Vec<u32> = [
			(0x02004033u32, "DIV"), (0x02005033, "DIVU"), (0x02006033, "REM"), (0x02007033, "REMU"),
			(0x0200003b, "MULW"), (0x0200403b, "DIVW"), (0x0200503b, "DIVUW"), (0x0200603b, "REMW"),
			(0x0200703b, "REMUW"),
		].iter().map(|&(base, _)| base | 5 << 7 | 6 << 15 | 7 << 20).collect();
		let edge: [i64; 10] = [0, 1, -1, 2, -7, i64::MIN, i64::MAX, i32::MIN as i64,
			0x1_0000_0000, -0x1_0000_0001];
		let mut r = Rng(77);
		let mut checked = 0;
		for &w in &words {
			for k in 0..60 {
				let mut cpu = fresh_cpu(&mut Rng(k + 1));
				let pick = |r: &mut Rng| match r.next() % 3 {
					0 => edge[(r.next() % 10) as usize],
					_ => r.next() as i64,
				};
				cpu.x[6] = pick(&mut r);
				cpu.x[7] = if k % 4 == 0 { 0 } else { pick(&mut r) };
				if k % 7 == 0 {
					cpu.x[6] = i64::MIN;
					cpu.x[7] = -1;
				}
				if k % 11 == 0 {
					cpu.x[6] = i32::MIN as i64;
					cpu.x[7] = -1;
				}
				let ops = vec![
					decode_op_for_test(&cpu, w),
					decode_op_for_test(&cpu, 0x0ff0000f), // fence
					decode_op_for_test(&cpu, 0x0000100f), // fence.i
					op(HOT_ADDI, 28, 5, 0, 1),
				];
				assert!(ops[..3].iter().all(|o| o.kind == 0 && jit::translatable(o)));
				let lay = flat_layout(&cpu);
				let bytes = jit::emit_block(&ops, DRAM_BASE, &lay).expect("emits");
				let mut m = flat_mem(&engine, &cpu, DRAM_BASE);
				let rw = m.call_block(&engine, &bytes);
				let (xw, pcw, _, _) = flat_state(&m);
				cpu.update_pc(DRAM_BASE);
				let ri = region_ref(&mut cpu, &[(DRAM_BASE, ops.clone())], 1 << 20, None);
				assert_eq!((rw, xw, pcw), (ri, cpu.x, cpu.pc), "{} x6={:#x} x7={:#x}",
					op_name(&ops[0]), cpu.x[6], cpu.x[7]);
				checked += 1;
			}
		}
		assert_eq!(checked, 540);
	}

	/// The fcsr cell and the LR/SC reservation, as a flat module left them.
	fn flat_extra(m: &Mem) -> (u64, u8, u64) {
		(m.get64(FCSRA), m.get(RESF, 1)[0], m.get64(RESA))
	}

	/// The word of INSTRUCTIONS entry `name` with register fields filled in
	/// wherever its mask leaves them free (rd, rs1, rs2, rs3).
	fn word_of(name: &str, rd: u32, rs1: u32, rs2: u32, rs3: u32) -> u32 {
		let i = INSTRUCTIONS.iter().find(|i| i.name == name).unwrap_or_else(|| panic!("{}", name));
		let fields = rd << 7 | rs1 << 15 | rs2 << 20 | rs3 << 27;
		i.data | (fields & !i.mask)
	}

	/// Doubles the float rules single out, as bits: ±0, ±1, ±inf, the
	/// canonical NaN, x86's negative default NaN, a quiet NaN with a
	/// payload, signaling NaNs of either sign, the smallest subnormal, the
	/// largest finite, 2.5 and -1.5 (rounding ties), 0.49999999999999994.
	const SPECIAL_D: [u64; 16] = [
		0x0000_0000_0000_0000, 0x8000_0000_0000_0000, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000,
		0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000, 0x7ff8_0000_0000_0000, 0xfff8_0000_0000_0000,
		0x7ff8_0000_dead_beef, 0x7ff0_0000_0000_0001, 0xfff4_0000_0000_0000, 0x0000_0000_0000_0001,
		0x7fef_ffff_ffff_ffff, 0x4004_0000_0000_0000, 0xbff8_0000_0000_0000, 0x3fdf_ffff_ffff_ffff,
	];
	/// The same for singles (the single's bits, before boxing).
	const SPECIAL_S: [u32; 16] = [
		0x0000_0000, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000,
		0xffc0_0000, 0x7fc0_1234, 0x7f80_0001, 0xffa0_0000, 0x0000_0001, 0x7f7f_ffff, 0x4020_0000,
		0xbfc0_0000, 0x3eff_ffff,
	];
	/// Registers that do NOT hold a properly boxed single (a single op reads
	/// each as the canonical NaN): a bare single, a double NaN, one bit
	/// short of a box, a double.
	const UNBOXED: [u64; 4] = [0x0000_0000_3f80_0000, 0x7ff8_0000_0000_0000, 0xffff_fffe_4020_0000,
		0x4000_0000_0000_0000];
	/// Float -> int inputs: halves and near-halves for every rounding mode,
	/// both sides of every integer range's edges.
	const CONV_D: [f64; 30] = [0.5, -0.5, 1.5, -2.5, 3.5, -0.3, 0.49999999999999994,
		-0.49999999999999994, 4503599627370495.5, -4503599627370495.5, 9007199254740991.0,
		2147483647.0, 2147483647.5, 2147483648.0, -2147483648.0, -2147483648.5, -2147483649.0,
		4294967295.0, 4294967295.5, 4294967296.0, 9223372036854775808.0, -9223372036854775808.0,
		9223372036854774784.0, -9223372036854777856.0, 18446744073709549568.0,
		18446744073709551616.0, -1.0, 1e-310, -1e-310, 1e300];
	const CONV_S: [f32; 24] = [0.5, -0.5, 1.5, -2.5, 3.5, -0.3, 0.49999997, 8388607.5, -8388607.5,
		2147483520.0, 2147483648.0, -2147483648.0, -2147483904.0, 4294967040.0, 4294967296.0,
		9223371487098961920.0, 9223372036854775808.0, -9223372036854775808.0,
		-9223373136366403584.0, 18446742974197923840.0, 18446744073709551616.0, -1.0, 1e-40, 3e38];

	/// Floats worth comparing: zeros, ones, fractions, large and tiny,
	/// infinities - and NaNs, quiet and signaling (SPECIAL_D): both sides
	/// canonicalize NaN results, so their payloads no longer differ.
	fn f64_pick(r: &mut Rng) -> f64 {
		let v = [0.0, -0.0, 1.0, -1.0, 0.5, -2.5, 3.75, 1e300, -1e-300, 2147483648.0,
			-2147483649.0, 9.3e18, -9.3e18, 1.8e19, 4294967295.5, f64::INFINITY, f64::NEG_INFINITY];
		match r.next() % 4 {
			0 => f64::from_bits(SPECIAL_D[(r.next() % SPECIAL_D.len() as u64) as usize]),
			1 => v[(r.next() % v.len() as u64) as usize],
			_ => ((r.next() % 4_000_000) as f64 - 2_000_000.0) / 1000.0,
		}
	}
	/// A register holding a single: NaN-boxed, except one time in eight a
	/// garbage upper half (the single ops must read the canonical NaN then).
	fn f32_bits_pick(r: &mut Rng) -> u64 {
		let v = [0.0f32, -0.0, 1.0, -1.0, 0.5, -2.5, 3.75, 3e38, -1e-38, 2147483648.0,
			-2147483904.0, 9.3e18, -9.3e18, 1.8e19, 16777217.0, f32::INFINITY, f32::NEG_INFINITY];
		let bits = match r.next() % 4 {
			0 => SPECIAL_S[(r.next() % SPECIAL_S.len() as u64) as usize],
			1 => v[(r.next() % v.len() as u64) as usize].to_bits(),
			_ => (((r.next() % 4_000_000) as f32 - 2_000_000.0) / 1000.0).to_bits(),
		};
		match r.next() % 8 {
			0 => (r.next() & 0xffff_fffe_0000_0000) | bits as u64,
			_ => FP_BOX | bits as u64,
		}
	}

	/// Run `ops` (one block at DRAM_BASE) compiled and interpreted from the
	/// same state; compare everything either side can touch.
	fn same_as_interpreter(engine: &wasmtime::Engine, cpu: &mut Cpu, ops: &[BlockOp], what: &str) {
		let lay = flat_layout(cpu);
		let bytes = jit::emit_block(ops, DRAM_BASE, &lay).unwrap_or_else(|| panic!("{} emits", what));
		let mut m = flat_mem(engine, cpu, DRAM_BASE);
		let rw = m.call_region(engine, &bytes, 1 << 20, 0);
		let (xw, pcw, dramw, fw) = flat_state(&m);
		let extra_w = flat_extra(&m);
		cpu.update_pc(DRAM_BASE);
		let ri = region_ref(cpu, &[(DRAM_BASE, ops.to_vec())], 1 << 20, None);
		let mut dram_i = vec![0u8; WIN as usize];
		cpu.mmu.read_physical_range(DRAM_BASE, &mut dram_i);
		let mut fi = [0u64; 32];
		for i in 0..32 {
			fi[i] = cpu.f[i].to_bits();
		}
		let extra_i = (cpu.csr[CSR_FCSR_ADDRESS as usize], cpu.is_reservation_set as u8, cpu.reservation);
		assert_eq!((rw, pcw), (ri, cpu.pc), "{}: retired/pc", what);
		assert_eq!(xw, cpu.x, "{}: x", what);
		assert_eq!(fw, fi, "{}: f", what);
		assert_eq!(extra_w, extra_i, "{}: fcsr/reservation", what);
		assert!(dramw == dram_i, "{}: dram", what);
	}

	/// MULH/MULHSU/MULHU and every AMO (including the four this change
	/// adds to the interpreter), on edge-heavy operands, rd aliasing the
	/// sources included.
	#[test]
	fn mulh_and_amos_match_the_interpreter() {
		let engine = engine();
		let edge: [i64; 12] = [0, 1, -1, 2, -7, i64::MIN, i64::MAX, i32::MIN as i64, i32::MAX as i64,
			0x1_0000_0000, -0x1_0000_0001, 0x7fff_ffff_ffff_fff0];
		let mut r = Rng(1234);
		let names = ["MULH", "MULHSU", "MULHU", "AMOADD.W", "AMOADD.D", "AMOSWAP.W", "AMOSWAP.D",
			"AMOXOR.W", "AMOXOR.D", "AMOOR.W", "AMOOR.D", "AMOAND.W", "AMOAND.D", "AMOMIN.W", "AMOMIN.D",
			"AMOMAX.W", "AMOMAX.D", "AMOMINU.W", "AMOMINU.D", "AMOMAXU.W", "AMOMAXU.D"];
		let mut checked = 0;
		for name in names.iter() {
			let amo = name.starts_with("AMO");
			for k in 0..80u64 {
				let mut cpu = fresh_cpu(&mut Rng(k * 7 + 3));
				let pick = |r: &mut Rng| match r.next() % 3 {
					0 => edge[(r.next() % edge.len() as u64) as usize],
					_ => r.next() as i64,
				};
				// rd: a fresh register, or one of the sources
				let (rd, rs1, rs2) = match k % 4 {
					0 => (6, 6, 7),
					1 => (7, 6, 7),
					_ => (5, 6, 7),
				};
				cpu.x[7] = pick(&mut r);
				match amo {
					true => {
						let at = DRAM_BASE + 8192 + (r.next() % 4096 & !7);
						cpu.x[6] = at as i64;
						let v = pick(&mut r) as u64;
						let _ = cpu.mmu.store_doubleword(at, v);
					}
					false => cpu.x[6] = pick(&mut r),
				}
				let w = word_of(name, rd, rs1, rs2, 0);
				let ops = vec![decode_op_for_test(&cpu, w), op(HOT_ADDI, 28, rd as u8, 0, 1)];
				assert!(ops[0].kind == 0 && jit::translatable(&ops[0]), "{}", name);
				same_as_interpreter(&engine, &mut cpu, &ops, &format!("{} case {}", name, k));
				checked += 1;
			}
		}
		assert_eq!(checked, 21 * 80);
	}

	/// LR/SC: a reservation taken and used in one block, an SC with no
	/// reservation, an SC for another address, an LR after an SC; the
	/// reservation's flag and address are compared as well as the memory.
	#[test]
	fn lr_sc_match_the_interpreter() {
		let engine = engine();
		let mut r = Rng(99);
		let mut checked = 0;
		for &wide in &[true, false] {
			let (lr, sc) = if wide { ("LR.D", "SC.D") } else { ("LR.W", "SC.W") };
			for k in 0..120u64 {
				let mut cpu = fresh_cpu(&mut Rng(k + 11));
				let at = DRAM_BASE + 8192 + (r.next() % 2048 & !7);
				let other = at + 64;
				cpu.x[6] = at as i64;
				cpu.x[9] = other as i64;
				cpu.is_reservation_set = r.next() % 2 == 0;
				cpu.reservation = match r.next() % 3 {
					0 => at,
					1 => other,
					_ => r.next(),
				};
				let addi = |rd: u8, rs: u8, imm: i32| op(HOT_ADDI, rd, rs, 0, imm);
				let ops = match k % 5 {
					// lr; add; sc - the CAS loop body
					0 => vec![decode_op_for_test(&cpu, word_of(lr, 5, 6, 0, 0)), addi(5, 5, 3),
						decode_op_for_test(&cpu, word_of(sc, 8, 6, 5, 0)), addi(28, 8, 1)],
					// sc alone: whatever reservation the state holds
					1 => vec![decode_op_for_test(&cpu, word_of(sc, 8, 6, 7, 0)), addi(28, 8, 1)],
					// sc to the other address
					2 => vec![decode_op_for_test(&cpu, word_of(sc, 8, 9, 7, 0)), addi(28, 8, 1)],
					// lr, then sc elsewhere (fails, drops it), then sc here (fails)
					3 => vec![decode_op_for_test(&cpu, word_of(lr, 5, 6, 0, 0)),
						decode_op_for_test(&cpu, word_of(sc, 8, 9, 7, 0)),
						decode_op_for_test(&cpu, word_of(sc, 10, 6, 7, 0)), addi(28, 10, 1)],
					// lr with rd == rs1: the reservation keeps the OLD address
					_ => vec![decode_op_for_test(&cpu, word_of(lr, 6, 6, 0, 0)), addi(28, 6, 1)],
				};
				same_as_interpreter(&engine, &mut cpu, &ops, &format!("{} case {}", lr, k));
				checked += 1;
			}
		}
		assert_eq!(checked, 240);
	}

	/// Every float op the translator takes from the table, single and
	/// double, plus FDIV.D (hot) and the float CSRs: operand values from
	/// f64_pick / f32_bits_pick (NaNs and improperly boxed singles
	/// included), fcsr starting from random bits; the conversions' rm is the
	/// word's 0 (RNE) here - float_special_values_match_the_interpreter
	/// takes every rm.
	#[test]
	fn float_ops_and_float_csrs_match_the_interpreter() {
		let engine = engine();
		let mut r = Rng(4242);
		// (name, single precision)
		let fops: &[(&str, bool)] = &[
			("FADD.S", true), ("FSUB.S", true), ("FMUL.S", true), ("FDIV.S", true),
			("FSQRT.S", true), ("FSGNJ.S", true), ("FSGNJN.S", true), ("FSGNJX.S", true),
			("FEQ.S", true), ("FLT.S", true), ("FLE.S", true), ("FCVT.W.S", true),
			("FCVT.WU.S", true), ("FCVT.L.S", true), ("FCVT.LU.S", true), ("FCVT.S.W", true),
			("FCVT.S.WU", true), ("FCVT.S.L", true), ("FCVT.S.LU", true), ("FMV.X.W", true),
			("FMV.W.X", true), ("FCVT.D.S", true),
			("FSQRT.D", false), ("FSGNJN.D", false), ("FSGNJX.D", false), ("FEQ.D", false),
			("FLT.D", false), ("FLE.D", false), ("FCVT.W.D", false), ("FCVT.WU.D", false),
			("FCVT.L.D", false), ("FCVT.LU.D", false), ("FCVT.D.WU", false), ("FCVT.D.L", false),
			("FCVT.D.LU", false), ("FCVT.S.D", false), ("FDIV.D", false),
		];
		let mut checked = 0;
		for &(name, single) in fops.iter() {
			for k in 0..60u64 {
				let mut cpu = fresh_cpu(&mut Rng(k * 13 + 1));
				for i in 0..32 {
					cpu.f[i] = match single {
						true => f64::from_bits(f32_bits_pick(&mut r)),
						false => f64_pick(&mut r),
					};
				}
				if k % 6 == 0 {
					// a zero divisor (and FDIV.D's -0.0 case)
					cpu.f[7] = match (single, k % 12 == 0) {
						(true, _) => f64::from_bits(FP_BOX | (k % 12 == 0) as u64 * 0x8000_0000),
						(false, true) => -0.0,
						(false, false) => 0.0,
					};
				}
				// integers for the int -> float conversions
				cpu.x[6] = match r.next() % 3 {
					0 => [0, -1, 1, i64::MIN, i64::MAX, u32::MAX as i64, i32::MIN as i64][(r.next() % 7) as usize],
					_ => r.next() as i64,
				};
				cpu.csr[CSR_FCSR_ADDRESS as usize] = r.next() & 0xff;
				let rd = if k % 5 == 0 { 6 } else { 5 };
				let w = word_of(name, rd, 6, 7, 28);
				let first = decode_op_for_test(&cpu, w);
				assert!(jit::translatable(&first), "{}", name);
				let ops = vec![first, op(HOT_ADDI, 29, 5, 0, 1)];
				same_as_interpreter(&engine, &mut cpu, &ops, &format!("{} case {}", name, k));
				checked += 1;
			}
		}
		// the float CSRs: every op and form, on fflags/frm/fcsr, from
		// random fcsr bits (including bits above frm, which fcsr keeps raw)
		for (base, imm) in [(0x1073u32, false), (0x2073, false), (0x3073, false),
			(0x5073, true), (0x6073, true), (0x7073, true)]
		{
			for csr in 1u32..=3 {
				for k in 0..20u64 {
					let mut cpu = fresh_cpu(&mut Rng(k + 500));
					cpu.csr[CSR_FCSR_ADDRESS as usize] = r.next() & if k % 2 == 0 { 0xff } else { 0xffff };
					let rd = [5u32, 6, 0][(k % 3) as usize];
					let src = if imm { (r.next() % 32) as u32 } else { 6 };
					let w = base | rd << 7 | src << 15 | csr << 20;
					let first = decode_op_for_test(&cpu, w);
					assert!(jit::translatable(&first));
					let ops = vec![first, op(HOT_ADDI, 29, 5, 0, 1)];
					same_as_interpreter(&engine, &mut cpu, &ops, &format!("{} csr {} case {}", op_name(&first), csr, k));
					checked += 1;
				}
			}
		}
		assert_eq!(checked, fops.len() * 60 + 6 * 3 * 20);
	}

	/// The float rules at their edges, translator against interpreter bit
	/// for bit, fcsr included (fflags start clear, so every flag an op
	/// raises shows): SPECIAL_D / SPECIAL_S (+ UNBOXED registers) pairwise
	/// through every translated binary op, rd aliasing rs1 in some cases;
	/// the unary ops over all of them; the float -> int conversions over
	/// CONV_D / CONV_S in every static rounding mode; the int -> float
	/// conversions and FMV over integer edges; FLW/FSW of NaN patterns.
	#[test]
	fn float_special_values_match_the_interpreter() {
		let engine = engine();
		let mut checked = 0u64;
		let mut case = |w: u32, setup: &dyn Fn(&mut Cpu), fcsr: u64, what: String| {
			let mut cpu = fresh_cpu(&mut Rng(checked + 1));
			setup(&mut cpu);
			cpu.csr[CSR_FCSR_ADDRESS as usize] = fcsr;
			let first = decode_op_for_test(&cpu, w);
			assert!(jit::translatable(&first), "{}", what);
			let ops = vec![first, op(HOT_ADDI, 29, 5, 0, 1)];
			same_as_interpreter(&engine, &mut cpu, &ops, &what);
			checked += 1;
		};
		let s_regs: Vec<u64> = SPECIAL_S.iter().map(|&b| FP_BOX | b as u64).chain(UNBOXED.iter().cloned()).collect();
		let pairs: [(&[&str], &[u64]); 2] = [
			(&["FADD.D", "FSUB.D", "FMUL.D", "FDIV.D", "FSGNJ.D", "FSGNJN.D", "FSGNJX.D", "FEQ.D",
				"FLT.D", "FLE.D"], &SPECIAL_D),
			(&["FADD.S", "FSUB.S", "FMUL.S", "FDIV.S", "FSGNJ.S", "FSGNJN.S", "FSGNJX.S", "FEQ.S",
				"FLT.S", "FLE.S"], &s_regs),
		];
		for &(names, vals) in pairs.iter() {
			for name in names.iter() {
				for (i, &a) in vals.iter().enumerate() {
					for (j, &b) in vals.iter().enumerate() {
						let rd = if (i + j) % 5 == 0 { 6 } else { 5 };
						case(word_of(name, rd, 6, 7, 0), &|c: &mut Cpu| {
							c.f[6] = f64::from_bits(a);
							c.f[7] = f64::from_bits(b);
						}, ((i + j) as u64 % 8) << 5, format!("{} {:#x} {:#x}", name, a, b));
					}
				}
			}
		}
		// unary ops and float -> int in rounding modes 0-4 (RNE RTZ RDN RUP
		// RMM; DYN and 5/6 stay with the interpreter, see
		// translatable_matches_the_emitter)
		let d_all: Vec<u64> = SPECIAL_D.iter().cloned().chain(CONV_D.iter().map(|v| v.to_bits())).collect();
		let s_all: Vec<u64> = s_regs.iter().cloned().chain(CONV_S.iter().map(|v| FP_BOX | v.to_bits() as u64)).collect();
		let unary: [(&[&str], &[&str], &[u64]); 2] = [
			(&["FSQRT.D", "FCVT.S.D"], &["FCVT.W.D", "FCVT.WU.D", "FCVT.L.D", "FCVT.LU.D"], &d_all),
			(&["FSQRT.S", "FCVT.D.S", "FMV.X.W"], &["FCVT.W.S", "FCVT.WU.S", "FCVT.L.S", "FCVT.LU.S"], &s_all),
		];
		for &(plain, to_int, vals) in unary.iter() {
			for (i, &a) in vals.iter().enumerate() {
				let set = |c: &mut Cpu| c.f[6] = f64::from_bits(a);
				for name in plain.iter() {
					let rd = if i % 3 == 0 { 6 } else { 5 };
					case(word_of(name, rd, 6, 0, 0), &set, 0, format!("{} {:#x}", name, a));
				}
				for name in to_int.iter() {
					for rm in 0..5u32 {
						case(word_of(name, 5, 6, 0, 0) | rm << 12, &set, (i as u64 % 8) << 5,
							format!("{} rm {} {:#x}", name, rm, a));
					}
				}
			}
		}
		// int -> float and the moves in, over integer edges
		let ints: [i64; 12] = [0, 1, -1, i32::MIN as i64, i32::MAX as i64, u32::MAX as i64, i64::MIN,
			i64::MAX, (1 << 53) + 1, (1 << 24) + 1, 0x1_0000_0001, 0x7fc0_0001];
		for name in ["FCVT.S.W", "FCVT.S.WU", "FCVT.S.L", "FCVT.S.LU", "FCVT.D.W", "FCVT.D.WU",
			"FCVT.D.L", "FCVT.D.LU", "FMV.W.X", "FMV.D.X"].iter()
		{
			for &v in ints.iter() {
				case(word_of(name, 5, 6, 0, 0), &|c: &mut Cpu| c.x[6] = v, 0, format!("{} {:#x}", name, v));
			}
		}
		// FLW boxes whatever single it loads; FSW stores the low 32 bits
		// raw (no unboxing)
		for &bits in SPECIAL_S.iter() {
			case(word_of("FLW", 5, 10, 0, 0), &|c: &mut Cpu| {
				let at = c.x[10] as u64;
				c.mmu.store_word(at, bits).ok().unwrap();
			}, 0, format!("FLW {:#x}", bits));
		}
		for &reg in s_regs.iter() {
			case(word_of("FSW", 0, 10, 7, 0), &|c: &mut Cpu| c.f[7] = f64::from_bits(reg), 0,
				format!("FSW {:#x}", reg));
		}
		let n = 2 * 10 * 400 - 10 * (400 - 256) // the double pairs are 16 x 16
			+ (2 + 4 * 5) * 46 + (3 + 4 * 5) * 44 + 10 * 12 + 16 + 20;
		assert_eq!(checked, n as u64);
	}

	#[test]
	fn region_entry_out_of_range_runs_nothing() {
		let engine = engine();
		let cpu = fresh_cpu(&mut Rng(5));
		let bytes = jit::emit_region(&loop_region(DRAM_BASE), &flat_layout(&cpu)).unwrap();
		let mut m = flat_mem(&engine, &cpu, 0x1234);
		assert_eq!(m.call_region(&engine, &bytes, 1000, 2), 0);
		assert_eq!(m.get64(PCA), 0x1234, "state untouched");
	}

	#[test]
	fn tlb_tier_translates_hits_and_bails_misses() {
		// synthetic single-page TLB: virtual page V maps to physical page P
		// inside the DRAM window; everything else must bail.
		const SETS: u32 = 512;
		const T_RT: u64 = 0x2_0000; // read tags (512 * 8), clear of the window
		const T_RM: u64 = T_RT + 4096; // read metas (512 * 4)
		const T_RP: u64 = T_RM + 2048; // read ppns
		const T_MC: u64 = T_RP + 4096; // meta cache cell
		let vpage: u64 = 0x4000_2000; // arbitrary virtual page
		let ppage: u64 = DRAM_BASE + 0x3000; // physical page in DRAM
		let meta: u32 = 0xabcd_1234;
		let engine = engine();

		let mut cpu = fresh_cpu(&mut Rng(4242));
		cpu.x[10] = (vpage + 0x40) as i64; // pointer into the mapped page
		cpu.x[11] = 0x5000_0000; // pointer with NO mapping
		let ops_hit = vec![op(HOT_LD, 5, 10, 0, 8), op(HOT_SD, 0, 10, 6, 16)];
		let ops_miss = vec![op(HOT_ADDI, 5, 5, 0, 1), op(HOT_LD, 7, 11, 0, 0)];
		let mut lay = flat_layout(&cpu);
		lay.tlb = Some(jit::TlbLayout {
			sets: SETS,
			read_tags: T_RT, read_metas: T_RM, read_ppns: T_RP,
			// write set shares the arrays in this synthetic setup
			write_tags: T_RT, write_metas: T_RM, write_ppns: T_RP,
			meta_cache: T_MC,
		});
		let fill_tlb = |m: &mut Mem, cache: u32| {
			let set = ((vpage >> 12) & (SETS as u64 - 1)) as u64;
			m.put64(T_RT + set * 8, (vpage & !0xfff) | 1);
			m.put(T_RM + set * 4, &meta.to_le_bytes());
			m.put64(T_RP + set * 8, ppage & !0xfff);
			m.put(T_MC, &cache.to_le_bytes());
		};
		let mem = |cpu: &Cpu| {
			let mut m = Mem::new(&engine, false, 3);
			for i in 0..32 {
				m.put64(XB + i as u64 * 8, cpu.x[i] as u64);
			}
			m.put(GENA, &cpu.mmu.code_gen().to_le_bytes());
			m
		};

		// hit case: LD then SD through the mapping run to completion
		let bytes = jit::emit_block(&ops_hit, DRAM_BASE, &lay).unwrap();
		let mut m = mem(&cpu);
		fill_tlb(&mut m, meta);
		let lin = DB + (ppage - DRAM_BASE) + 0x40;
		m.put64(lin + 8, 0x1122_3344_5566_7788);
		assert_eq!(m.call_block(&engine, &bytes), 2, "hit case must complete");
		assert_eq!(m.get64(XB + 5 * 8), 0x1122_3344_5566_7788, "loaded through mapping");
		assert_eq!(m.get64(lin + 16) as i64, cpu.x[6], "stored through mapping");

		// miss case: first op runs, the unmapped LD bails with pc at it
		let bytes = jit::emit_block(&ops_miss, DRAM_BASE, &lay).unwrap();
		let mut m = mem(&cpu);
		fill_tlb(&mut m, meta);
		assert_eq!(m.call_block(&engine, &bytes), 1, "miss bails before the load");
		assert_eq!(m.get64(PCA), DRAM_BASE + 4, "pc at the bailing op");

		// stale meta: flip the cache cell; the mapped LD must now bail at op 0
		let bytes = jit::emit_block(&ops_hit, DRAM_BASE, &lay).unwrap();
		let mut m = mem(&cpu);
		fill_tlb(&mut m, meta.wrapping_add(1));
		assert_eq!(m.call_block(&engine, &bytes), 0, "stale meta bails immediately");

		// cross-page access: an LD at page offset 0xffc bails, never splits
		cpu.x[10] = (vpage + 0xffc - 8) as i64;
		let mut m = mem(&cpu);
		fill_tlb(&mut m, meta);
		let bytes = jit::emit_block(&ops_hit, DRAM_BASE, &lay).unwrap();
		assert_eq!(m.call_block(&engine, &bytes), 0, "cross-page access bails");
	}

	#[test]
	fn store_bails_on_stale_generation() {
		let engine = engine();
		let mut r = Rng(97);
		let cpu = fresh_cpu(&mut r);
		let ops = vec![
			op(HOT_ADDI, 5, 6, 0, 0),
			op(HOT_SD, 0, 10, 7, 0),
			op(HOT_ADDI, 8, 9, 0, 0),
		];
		let mut lay = flat_layout(&cpu);
		lay.baked_gen = cpu.mmu.code_gen().wrapping_add(1); // stale on purpose
		let bytes = jit::emit_block(&ops, DRAM_BASE, &lay).unwrap();
		let mut m = flat_mem(&engine, &cpu, DRAM_BASE);
		// the store executes, the gen check fires after it: 2 retired,
		// pc at the third op
		assert_eq!(m.call_block(&engine, &bytes), 2);
		assert_eq!(m.get64(PCA), DRAM_BASE + 8);
	}

	// ---- the production shape: memory64, chunked RAM, SV39, bias -------

	const RAM: u64 = 4 << 16; // 4 chunks
	const VBASE: u64 = 0x4000_0000; // mapped virtual window
	const VPAGES: u64 = 40;
	const PT_ROOT: u64 = DRAM_BASE + 0x3_0000; // page tables live in chunk 3
	const STORE_BAIL: (u64, u64) = (0x2_8000, 0x2_9000); // DRAM offsets
	// synthetic linear layout, all above 4 GiB except the context block
	const CTX64: u64 = 0x100;
	const HI: u64 = 0x1_0000_0000;
	const STATE: u64 = HI + 0x1_0000; // x +0, pc +0x100, f +0x200, TLB +0x1000..
	const RDT: u64 = HI + 0x2_0000;
	const WRT: u64 = HI + 0x2_1000;
	const MARKS: u64 = HI + 0x2_2000;
	const CHUNKS: u64 = HI + 0x10_0000; // chunk i at CHUNKS + i * 64 KiB
	const PAGES64: u64 = (CHUNKS + 8 * 0x1_0000) / 0x1_0000;

	fn s_x(i: usize) -> u64 { STATE + i as u64 * 8 }
	fn s_f(i: usize) -> u64 { STATE + 0x200 + i as u64 * 8 }
	const S_PC: u64 = 0x100;

	fn chunked_layout(memory64: bool) -> jit::Layout {
		jit::Layout {
			memory64,
			shared: false,
			max_pages: None,
			ctx: CTX64,
			x_base: 0,
			f_base: 0x200,
			pc_addr: S_PC,
			gen_addr: 0,
			baked_gen: 0,
			fcsr_addr: 0x108,
			res_flag_addr: 0x110,
			res_addr_addr: 0x118,
			tlb: Some(jit::TlbLayout {
				sets: 512,
				read_tags: 0x1000,
				read_metas: 0x2000,
				read_ppns: 0x3000,
				write_tags: 0x4000,
				write_metas: 0x5000,
				write_ppns: 0x6000,
				meta_cache: 0x7000,
			}),
			guest_dram_base: DRAM_BASE,
			dram_len: RAM,
			ram: jit::Ram::Chunked { store_bail: vec![STORE_BAIL] },
		}
	}

	/// A machine with SV39 paging on in S-mode: VPAGES virtual pages at
	/// VBASE map to shuffled physical pages of chunks 0..2 (some read-only,
	/// some unmapped), the TLB warmed for most of them (read and/or write),
	/// RAM part owned and part shared (copy-on-write), a few data pages
	/// marked executable. Deterministic per seed.
	fn paged_cpu(seed: u64) -> Cpu {
		let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
		let mut cpu = Cpu::new(Box::new(DummyTerminal::new()));
		cpu.get_mut_mmu().init_memory(RAM);
		for a in (0..RAM).step_by(8) {
			let v = r.next();
			let _ = cpu.mmu.store_doubleword(DRAM_BASE + a, v);
		}
		// page tables: root -> L1 -> L0, VBASE has vpn2 = 1, vpn1 = 0
		let (l1, l0) = (PT_ROOT + 0x1000, PT_ROOT + 0x2000);
		for i in 0..512 * 3 {
			let _ = cpu.mmu.store_doubleword(PT_ROOT + i * 8, 0);
		}
		let _ = cpu.mmu.store_doubleword(PT_ROOT + 8, ((l1 >> 12) << 10) | 1);
		let _ = cpu.mmu.store_doubleword(l1, ((l0 >> 12) << 10) | 1);
		let mut phys: Vec<u64> = (0..48).collect(); // pages of chunks 0..2
		for i in (1..phys.len()).rev() {
			let j = (r.next() % (i as u64 + 1)) as usize;
			phys.swap(i, j);
		}
		let mut writable = vec![false; VPAGES as usize];
		for i in 0..VPAGES as usize {
			let pa = DRAM_BASE + phys[i] * 0x1000;
			let pte = match r.next() % 8 {
				0 => 0, // unmapped
				1 => ((pa >> 12) << 10) | 0x43, // V R A: read-only
				_ => {
					writable[i] = true;
					((pa >> 12) << 10) | 0xc7 // V R W A D
				}
			};
			let _ = cpu.mmu.store_doubleword(l0 + i as u64 * 8, pte);
		}
		// one more page maps device space (the UART): reachable through
		// the TLB, but never DRAM
		let _ = cpu.mmu.store_doubleword(l0 + VPAGES * 8, ((0x1000_0000u64 >> 12) << 10) | 0xc7);
		cpu.update_addressing_mode((8 << 60) | (PT_ROOT >> 12));
		cpu.privilege_mode = PrivilegeMode::Supervisor;
		cpu.mmu.update_privilege_mode(PrivilegeMode::Supervisor);
		// pointer registers: mostly well inside a mapped page, sometimes at
		// an unaligned offset near its end (accesses straddle the page)
		let mut ptr_pages = [0u64; 4];
		for p in 10..14 {
			let page = r.next() % VPAGES;
			let off = match r.next() % 6 {
				0 => 0xff0,
				1 => 0xffb,
				2 => r.next() % 0x700,
				_ => r.next() % 0x700 & !7,
			};
			ptr_pages[p - 10] = page;
			cpu.x[p] = (VBASE + page * 0x1000 + off) as i64;
			// sometimes: the device page, or an UNMAPPED page 2 MiB up whose
			// TLB set holds this page's (fresh) entry — only the tag says no
			match (p, r.next() % 5) {
				(12, 0) => cpu.x[p] = (VBASE + VPAGES * 0x1000 + off) as i64,
				(13, 0) | (13, 1) => cpu.x[p] += 512 * 0x1000,
				_ => {}
			}
		}
		// warm the TLB: reads for most pages, writes for some writable ones
		let warm = |cpu: &mut Cpu, r: &mut Rng, skip: Option<u64>| {
			for i in 0..VPAGES {
				let va = VBASE + i * 0x1000;
				if Some(i) == skip {
					continue;
				}
				if r.next() % 6 != 0 {
					let _ = cpu.mmu.load_doubleword(va);
				}
				if writable[i as usize] && r.next() % 3 != 0 {
					if let Ok(v) = cpu.mmu.load_doubleword(va + 8) {
						let _ = cpu.mmu.store_doubleword(va + 8, v);
					}
				}
			}
		};
		warm(&mut cpu, &mut r, None);
		let _ = cpu.mmu.load_doubleword(VBASE + VPAGES * 0x1000);
		if seed % 2 == 0 {
			// A stale translation: remap x10's page to another frame after
			// its TLB entries were filled, SFENCE (new meta), and re-warm
			// everything else. The old entries keep a matching tag and the
			// OLD frame; only the meta says they are dead.
			let page = ptr_pages[0];
			let pte_at = l0 + page * 8;
			let pte = cpu.mmu.load_doubleword_phys(pte_at);
			if pte & 1 != 0 {
				let frame = DRAM_BASE + phys[VPAGES as usize + (r.next() % 8) as usize] * 0x1000;
				cpu.mmu.store_doubleword_phys(pte_at, ((frame >> 12) << 10) | (pte & 0x3ff));
				cpu.mmu.sfence_vma();
				warm(&mut cpu, &mut r, Some(page));
			}
		}
		// copy-on-write: share everything, then re-own two chunks
		let _image = cpu.mmu.share_ram();
		for c in 0..4u64 {
			if r.next() % 2 == 0 {
				let p = DRAM_BASE + c * 0x1_0000 + 0x10;
				let b = cpu.mmu.load_word_raw(p);
				cpu.mmu.store_raw(p, b as u8);
			}
		}
		// executable marks on a few data pages
		for _ in 0..3 {
			let _ = cpu.mmu.mark_exec_page(DRAM_BASE + phys[(r.next() % VPAGES) as usize] * 0x1000);
		}
		for i in 1..32 {
			if !(10..14).contains(&i) {
				cpu.x[i] = r.next() as i64;
			}
		}
		for i in 0..32 {
			cpu.f[i] = (r.next() % 2000000) as f64 / 1000.0 - 1000.0;
		}
		cpu
	}

	/// Serialize a paged, chunked machine into the synthetic memory64
	/// layout: registers and pc, the machine's REAL TLB ways and meta, its
	/// chunk tables (write pointers only where the machine owns the chunk)
	/// and exec-page marks, and the context block.
	fn chunked_mem(engine: &wasmtime::Engine, cpu: &Cpu, bias: u64) -> Mem {
		let mut m = Mem::new(engine, true, PAGES64);
		for i in 0..32 {
			m.put64(s_x(i), cpu.x[i] as u64);
			m.put64(s_f(i), cpu.f[i].to_bits());
		}
		m.put64(STATE + S_PC, cpu.pc);
		let (tlb, _) = cpu.mmu.jit_tlb();
		let raw = |a: u64, n: usize| unsafe { std::slice::from_raw_parts(a as usize as *const u8, n) }.to_vec();
		for (i, &(off, n)) in [(0x1000u64, 4096usize), (0x2000, 2048), (0x3000, 4096),
			(0x4000, 4096), (0x5000, 2048), (0x6000, 4096), (0x7000, 4)].iter().enumerate() {
			m.put(STATE + off, &raw(tlb[i], n));
		}
		let (rd, wr, marks, len) = cpu.mmu.jit_ram();
		assert_eq!(len, RAM);
		for c in 0..(RAM >> 16) {
			let mut data = vec![0u8; 0x1_0000];
			cpu.mmu.read_physical_range(DRAM_BASE + (c << 16), &mut data);
			let at = CHUNKS + c * 0x1_0000;
			m.put(at, &data);
			let owned = unsafe { !(*(wr as usize as *const *mut u8).add(c as usize)).is_null() };
			let _ = rd;
			m.put64(RDT + c * 8, at);
			m.put64(WRT + c * 8, if owned { at } else { 0 });
		}
		m.put(MARKS, &raw(marks, (RAM >> 12) as usize));
		m.put64(CTX64 + jit::CTX_BASE, STATE);
		m.put64(CTX64 + jit::CTX_RD, RDT);
		m.put64(CTX64 + jit::CTX_WR, WRT);
		m.put64(CTX64 + jit::CTX_MARKS, MARKS);
		m.put64(CTX64 + jit::CTX_BIAS, bias);
		m
	}

	/// The compiled side's architectural state vs a reference machine.
	fn assert_same(m: &Mem, cpu: &Cpu, what: &str) {
		for i in 0..32 {
			assert_eq!(m.get64(s_x(i)) as i64, cpu.x[i], "x{} {}", i, what);
			assert_eq!(m.get64(s_f(i)), cpu.f[i].to_bits(), "f{} {}", i, what);
		}
		assert_eq!(m.get64(STATE + S_PC), cpu.pc, "pc {}", what);
		for c in 0..(RAM >> 16) {
			let mut data = vec![0u8; 0x1_0000];
			cpu.mmu.read_physical_range(DRAM_BASE + (c << 16), &mut data);
			assert!(m.get(CHUNKS + c * 0x1_0000, 0x1_0000) == data, "chunk {} {}", c, what);
		}
	}

	/// Run `blocks` (module pcs) compiled at `bias` from entry 0 and check
	/// it against the reference — fully when the module ran to a region
	/// exit, else at the exact op it bailed before. Returns (retired by the
	/// module, whether it bailed early).
	fn check_region(engine: &wasmtime::Engine, seed: u64, blocks: &[(u64, Vec<BlockOp>)], bias: u64,
		fuel: u64, setup: &dyn Fn(&mut Cpu)) -> (u64, bool)
	{
		let lay = chunked_layout(true);
		let bytes = jit::emit_region(blocks, &lay).expect("region emits");
		let rt: Vec<(u64, Vec<BlockOp>)> = blocks.iter().map(|b| (b.0.wrapping_add(bias), b.1.clone())).collect();
		let fresh = || {
			let mut c = paged_cpu(seed);
			setup(&mut c);
			c.update_pc(rt[0].0);
			c
		};
		let cpu = fresh();
		let mut m = chunked_mem(engine, &cpu, bias);
		let rw = m.call_region(engine, &bytes, fuel, 0);
		let mut full = fresh();
		let ri = region_ref(&mut full, &rt, fuel, None);
		if ri == rw && full.pc == m.get64(STATE + S_PC) {
			assert_same(&m, &full, &format!("seed {} (completed, {} retired)", seed, rw));
			return (rw, false);
		}
		assert!(rw < ri, "seed {}: module retired {} past the reference's {}", seed, rw, ri);
		let mut part = fresh();
		let rp = region_ref(&mut part, &rt, fuel, Some(rw));
		assert_eq!(rp, rw, "seed {}", seed);
		assert_same(&m, &part, &format!("seed {} (bailed after {} of {})", seed, rw, ri));
		(rw, true)
	}

	#[test]
	fn chunked_paged_blocks_match_interpreter() {
		let engine = engine();
		let (mut completed, mut bailed, mut ops_run) = (0, 0, 0u64);
		for seed in 1..300u64 {
			let mut r = Rng(seed * 7919 | 1);
			let len = 2 + (r.next() % 14) as usize;
			let mut ops = rand_ops(&mut r, len);
			// small, unaligned displacements: with the pointers' near-end
			// offsets these straddle pages
			for o in ops.iter_mut() {
				let mem = matches!(o.kind, HOT_SB..=HOT_FSD | HOT_LD | HOT_LW | HOT_LWU | HOT_LH
					| HOT_LHU | HOT_LB | HOT_LBU | HOT_FLD | HOT_FLW);
				if mem && r.next() % 3 == 0 {
					o.imm = (r.next() % 8) as i32;
				}
			}
			let bias = match seed % 3 {
				0 => 0,
				_ => (r.next() & 0x3f_ffff) << 12,
			};
			let rel = 0x2000 + (r.next() % 0x300) * 4;
			// fuel-bounded: a block branching to its own start loops in-region
			let (rw, b) = check_region(&engine, seed, &[(rel, ops)], bias, 500, &|_| {});
			ops_run += rw;
			if b { bailed += 1 } else { completed += 1 }
		}
		eprintln!("chunked blocks: {} completed, {} bailed, {} ops compiled-run", completed, bailed, ops_run);
		assert!(completed > 120 && bailed > 20, "coverage of both paths: {} / {}", completed, bailed);
	}

	/// A random region: 2..6 member blocks (module pcs) at a nonzero bias,
	/// each ending in a branch, JAL or JALR to another member (or out), the
	/// RUNTIME targets x5..x7 hold for the JALRs (members, or not), a fuel
	/// bound, and how many ops the blocks hold before their terminators.
	struct RandRegion {
		blocks: Vec<(u64, Vec<BlockOp>)>,
		bias: u64,
		targets: Vec<u64>,
		fuel: u64,
		body_len: usize,
	}

	fn rand_region(r: &mut Rng) -> RandRegion {
		let n = 2 + (r.next() % 5) as usize;
		let starts: Vec<u64> = (0..n as u64).map(|i| 0x1000 + i * 0x100 + (r.next() % 8) * 4).collect();
		let bias = (1 + (r.next() & 0xffff)) << 12;
		let mut blocks = Vec::new();
		let mut body_len = 0;
		for (i, &s) in starts.iter().enumerate() {
			let len = 1 + (r.next() % 6) as usize;
			let mut ops = rand_ops(r, len);
			// keep control flow for the terminator
			ops.retain(|o| !matches!(o.kind, HOT_BEQ..=HOT_BGEU | HOT_JAL | HOT_JALR));
			if ops.is_empty() {
				ops.push(op(HOT_ADDI, 28, 28, 0, 1));
			}
			body_len += ops.len();
			let here = s + ops.len() as u64 * 4;
			let to = starts[(r.next() as usize) % n];
			let rel = (to.wrapping_sub(here)) as i32;
			let term = match r.next() % 5 {
				0 => op(HOT_BNE, 0, 28, 0, rel),
				1 => op(HOT_BEQ, 0, 0, 0, rel), // always taken
				2 => op(HOT_JAL, if r.next() % 2 == 0 { 1 } else { 0 }, 0, 0, rel),
				3 => op(HOT_JALR, 1, 5 + (r.next() % 3) as u8, 0, 0),
				_ => op(HOT_ADDI, 29, 29, 0, i as i32), // fall out
			};
			ops.push(term);
			blocks.push((s, ops));
		}
		let targets: Vec<u64> = (0..3).map(|_| match r.next() % 4 {
			0 => bias + 0x1000 + 0x8000, // not a member
			_ => bias + starts[(r.next() as usize) % n],
		}).collect();
		let fuel = 20 + r.next() % 400;
		RandRegion { blocks, bias, targets, fuel, body_len }
	}

	/// Random regions (rand_region): in-region transfers, indirect dispatch
	/// and fuel exits against the reference.
	#[test]
	fn chunked_regions_with_bias_and_indirect_jumps_match() {
		let engine = engine();
		let (mut transfers, mut cases) = (0, 0);
		for seed in 1..200u64 {
			let mut r = Rng(seed * 104729 | 1);
			let rr = rand_region(&mut r);
			let targets = rr.targets.clone();
			let setup = move |c: &mut Cpu| {
				for k in 0..3 {
					c.x[5 + k] = targets[k] as i64;
				}
			};
			let (rw, _) = check_region(&engine, seed, &rr.blocks, rr.bias, rr.fuel, &setup);
			if rw > rr.body_len as u64 {
				transfers += 1;
			}
			cases += 1;
		}
		// a coverage floor, not a correctness check: the hostile pointers
		// (device page, aliasing sets) make many cases bail early by design
		assert!(transfers > cases / 6, "in-region transfers exercised: {} of {}", transfers, cases);
	}

	/// A call and return inside one region: JAL into a helper block, JALR
	/// back through the indirect dispatcher to the continuation block, a
	/// loop around both — the whole thing must stay compiled.
	#[test]
	fn call_and_return_stay_in_region() {
		let engine = engine();
		let base = 0x3000u64;
		// a: x6 -= 1; call helper | cont (= the return address): loop to a
		let (a, cont, helper) = (base, base + 8, base + 0x40);
		let blocks = vec![
			(a, vec![op(HOT_ADDI, 6, 6, 0, -1), op(HOT_JAL, 1, 0, 0, (helper - (a + 4)) as i32)]),
			(cont, vec![op(HOT_BNE, 0, 6, 0, (a as i64 - cont as i64) as i32)]),
			(helper, vec![op(HOT_ADD, 28, 28, 6, 0), op(HOT_JALR, 0, 1, 0, 0)]),
		];
		let bias = 0x7f00_0000;
		let (rw, bailed) = check_region(&engine, 11, &blocks, bias, 1 << 30, &|c| {
			c.x[6] = 50;
			c.x[28] = 0;
		});
		assert!(!bailed);
		assert_eq!(rw, 50 * 5, "every iteration ran compiled");
	}

	// ---- packed modules: several regions behind one run(fuel, entry) -----

	const S_FCSR: u64 = 0x108;

	/// Everything a chunked module can change: x, f, pc, fcsr, every chunk.
	fn chunked_state(m: &Mem) -> (Vec<u64>, Vec<u64>, u64, u64, Vec<u8>) {
		let x = (0..32).map(|i| m.get64(s_x(i))).collect();
		let f = (0..32).map(|i| m.get64(s_f(i))).collect();
		(x, f, m.get64(STATE + S_PC), m.get64(STATE + S_FCSR), m.get(CHUNKS, (RAM) as usize))
	}

	/// Enter random region `rr` at its block `i`: through entry `entry` of
	/// the PACKED module and through entry i of the module compiled ALONE,
	/// both at rr.bias, then against the reference interpreter — fully when
	/// the modules ran to a region exit, else at the exact op they bailed
	/// before. The two modules must agree bit for bit (retired, x, f, pc,
	/// fcsr, memory); the reference on all of it too. Returns (retired,
	/// whether it bailed early).
	fn check_entry(engine: &wasmtime::Engine, seed: u64, rr: &RandRegion, i: usize, packed: &wasmtime::Module,
		entry: u32, alone: &wasmtime::Module) -> (u64, bool)
	{
		let rt: Vec<(u64, Vec<BlockOp>)> = rr.blocks.iter().map(|b| (b.0.wrapping_add(rr.bias), b.1.clone())).collect();
		let fresh = || {
			let mut c = paged_cpu(seed);
			for k in 0..3 {
				c.x[5 + k] = rr.targets[k] as i64;
			}
			c.update_pc(rt[i].0);
			c
		};
		let what = format!("seed {} block {} (entry {})", seed, i, entry);
		let cpu = fresh();
		let mut mp = chunked_mem(engine, &cpu, rr.bias);
		let rp = mp.call_module(packed, rr.fuel, entry);
		let mut ma = chunked_mem(engine, &cpu, rr.bias);
		let ra = ma.call_module(alone, rr.fuel, i as u32);
		assert_eq!(rp, ra, "{}: retired, packed vs alone", what);
		assert!(chunked_state(&mp) == chunked_state(&ma), "{}: state, packed vs alone", what);
		let mut full = fresh();
		let ri = region_ref(&mut full, &rt, rr.fuel, None);
		let fcsr = mp.get64(STATE + S_FCSR);
		if ri == rp && full.pc == mp.get64(STATE + S_PC) {
			assert_same(&mp, &full, &format!("{} (completed, {} retired)", what, rp));
			assert_eq!(fcsr, full.csr[CSR_FCSR_ADDRESS as usize], "{}: fcsr", what);
			return (rp, false);
		}
		assert!(rp < ri, "{}: module retired {} past the reference's {}", what, rp, ri);
		let mut part = fresh();
		assert_eq!(region_ref(&mut part, &rt, rr.fuel, Some(rp)), rp, "{}", what);
		assert_same(&mp, &part, &format!("{} (bailed after {} of {})", what, rp, ri));
		assert_eq!(fcsr, part.csr[CSR_FCSR_ADDRESS as usize], "{}: fcsr", what);
		(rp, true)
	}

	/// A pack of three random regions: entering EVERY entry of every group
	/// (base_k + i, with that group's bias) gives what the same region
	/// compiled alone gives, bit for bit, and what the interpreter gives.
	/// Each region draws its own bias (different per group); one case in
	/// three moves all three to the SAME bias, so the groups' runtime pcs
	/// interleave and a fallthrough of one often lands on a block start of
	/// another (which it must leave to, as its own module would). Entries
	/// past the last group run nothing.
	#[test]
	fn packed_regions_match_alone_and_interpreter() {
		let engine = engine();
		let lay = chunked_layout(true);
		let (mut entries, mut bailed, mut transfers, mut shared_bias) = (0u64, 0u64, 0u64, 0u64);
		let (mut packed_bytes, mut alone_bytes) = (0usize, 0usize);
		for seed in 1..80u64 {
			let mut r = Rng(seed * 15485863 | 1);
			let mut regions: Vec<RandRegion> = (0..3).map(|_| rand_region(&mut r)).collect();
			if seed % 3 == 0 {
				let b = regions[0].bias;
				for rr in regions.iter_mut() {
					for t in rr.targets.iter_mut() {
						*t = *t - rr.bias + b;
					}
					rr.bias = b;
				}
				shared_bias += 1;
			} else {
				assert!(regions[0].bias != regions[1].bias || regions[1].bias != regions[2].bias);
			}
			let groups: Vec<Vec<(u64, Vec<BlockOp>)>> = regions.iter().map(|rr| rr.blocks.clone()).collect();
			let (bytes, bases) = jit::emit_regions(&groups, &lay).expect("pack emits");
			let n: Vec<u32> = regions.iter().map(|rr| rr.blocks.len() as u32).collect();
			assert_eq!(bases, vec![0, n[0], n[0] + n[1]], "entry bases");
			let packed = wasmtime::Module::new(&engine, &bytes).expect("valid pack");
			packed_bytes += bytes.len();
			if seed == 1 {
				eprintln!("pack: {} groups, {} entries, {} bytes (alone: {} bytes)", groups.len(),
					n.iter().sum::<u32>(), bytes.len(),
					groups.iter().map(|g| jit::emit_region(g, &lay).unwrap().len()).sum::<usize>());
			}
			for (k, rr) in regions.iter().enumerate() {
				let one = jit::emit_region(&rr.blocks, &lay).expect("region emits");
				alone_bytes += one.len();
				let alone = wasmtime::Module::new(&engine, &one).unwrap();
				for i in 0..rr.blocks.len() {
					let (rw, b) = check_entry(&engine, seed, rr, i, &packed, bases[k] + i as u32, &alone);
					entries += 1;
					bailed += b as u64;
					if rw > rr.blocks[i].1.len() as u64 {
						transfers += 1;
					}
				}
			}
			// past the last entry: nothing runs, nothing changes
			let cpu = paged_cpu(seed);
			let mut m = chunked_mem(&engine, &cpu, regions[0].bias);
			let before = chunked_state(&m);
			assert_eq!(m.call_module(&packed, 1000, n.iter().sum::<u32>()), 0);
			assert!(chunked_state(&m) == before, "seed {}: out-of-range entry touched state", seed);
		}
		eprintln!("packed entries: {} checked ({} bailed early, {} ran past their first block), {} packs at one bias; \
			{} packed bytes vs {} alone", entries, bailed, transfers, shared_bias, packed_bytes, alone_bytes);
		assert!(entries > 600 && transfers > entries / 8 && entries - bailed > entries / 3,
			"coverage: {} entries, {} bailed, {} transfers", entries, bailed, transfers);
		// one module's header instead of three, a few bytes of group switch per entry
		assert!(packed_bytes < alone_bytes, "{} vs {}", packed_bytes, alone_bytes);
	}

	/// The packer's size arithmetic: a pack never exceeds PACK_FIXED_BOUND
	/// plus its groups' pack_cost (the packer fills against that bound), up
	/// to MAX_PACK_GROUPS groups and with a production-shaped layout (a
	/// context block at a high address, shared memory64 with a maximum);
	/// one group packs to exactly emit_region's module; a pack is a valid
	/// module; more than MAX_PACK_GROUPS groups is refused.
	#[test]
	fn pack_size_bound_and_single_group_identity() {
		let engine = engine();
		let mut prod = chunked_layout(true);
		prod.shared = true;
		prod.max_pages = Some(1 << 18);
		prod.ctx = 0x7fff_f7a3_1d48;
		let mut slack = usize::MAX;
		for (li, lay) in [chunked_layout(true), chunked_layout(false), prod].iter().enumerate() {
			for seed in 1..40u64 {
				let mut r = Rng(seed * 7_368_787 | 1);
				let k = match seed {
					1 => jit::MAX_PACK_GROUPS,
					_ => 1 + (r.next() % 12) as usize,
				};
				let regions: Vec<RandRegion> = (0..k).map(|_| rand_region(&mut r)).collect();
				let groups: Vec<jit::GroupCode> = regions.iter().map(|rr| jit::emit_group(&rr.blocks, lay).unwrap()).collect();
				let refs: Vec<&jit::GroupCode> = groups.iter().collect();
				let (bytes, bases) = jit::pack(&refs, lay).expect("packs");
				let bound = jit::PACK_FIXED_BOUND + groups.iter().map(|g| g.pack_cost()).sum::<usize>();
				assert!(bytes.len() <= bound, "layout {} seed {}: {} > {}", li, seed, bytes.len(), bound);
				slack = slack.min(bound - bytes.len());
				let blocks: Vec<Vec<(u64, Vec<BlockOp>)>> = regions.iter().map(|rr| rr.blocks.clone()).collect();
				assert_eq!(jit::emit_regions(&blocks, lay), Some((bytes.clone(), bases)), "emit_regions = pack(emit_group)");
				if k == 1 {
					assert_eq!(Some(bytes.clone()), jit::emit_region(&regions[0].blocks, lay), "one group = emit_region");
				}
				if seed == 1 || seed == 2 {
					// shared memories need the threads proposal; validate, don't instantiate
					wasmtime::Module::validate(&engine, &bytes).expect("valid pack");
				}
			}
		}
		let g = jit::emit_group(&loop_region(0), &chunked_layout(true)).unwrap();
		let too_many: Vec<&jit::GroupCode> = (0..jit::MAX_PACK_GROUPS + 1).map(|_| &g).collect();
		assert!(jit::pack(&too_many, &chunked_layout(true)).is_none());
		eprintln!("pack size bound: at least {} bytes to spare", slack);
	}

	/// A group's control transfers reach ONLY its own blocks. Group A's
	/// blocks leave by a JAL, a taken branch, a JALR and a fallthrough, each
	/// to a pc where group B — same module, same bias, so the very same
	/// runtime pcs — starts a block; and B's JAL goes to A's first block.
	/// Every one of them must leave the module with pc at the target and
	/// nothing of the other group run. The same blocks as ONE group do run
	/// on (the transfers are real), so this fails if the scoping breaks.
	#[test]
	fn packed_group_never_enters_another_groups_block() {
		let engine = engine();
		let lay = chunked_layout(true);
		let bias = 0x7f00_0000u64;
		let a: Vec<(u64, Vec<BlockOp>)> = vec![
			(0x1000, vec![op(HOT_ADDI, 28, 28, 0, 1), op(HOT_JAL, 0, 0, 0, 0x2000 - 0x1004)]),
			(0x1100, vec![op(HOT_ADDI, 28, 28, 0, 2), op(HOT_BEQ, 0, 0, 0, 0x2000 - 0x1104)]),
			(0x1200, vec![op(HOT_ADDI, 28, 28, 0, 3), op(HOT_JALR, 0, 5, 0, 0)]),
			(0x1300, vec![op(HOT_ADDI, 28, 28, 0, 4), op(HOT_ADDI, 28, 28, 0, 5)]), // falls through to 0x1308
		];
		let b: Vec<(u64, Vec<BlockOp>)> = vec![
			(0x1308, vec![op(HOT_ADDI, 29, 29, 0, 200), op(HOT_ADDI, 29, 29, 0, 1)]), // falls out at 0x1310
			(0x2000, vec![op(HOT_ADDI, 29, 29, 0, 100), op(HOT_JAL, 0, 0, 0, 0x1000 - 0x2004)]),
		];
		// (group, block, retired, exit pc) for each entry of the pack
		let expect: [(usize, usize, u64, u64); 6] = [
			(0, 0, 2, 0x2000), (0, 1, 2, 0x2000), (0, 2, 2, 0x2000), (0, 3, 2, 0x1308),
			(1, 0, 2, 0x1310), (1, 1, 2, 0x1000),
		];
		let setup = |c: &mut Cpu| {
			c.x[5] = (bias + 0x2000) as i64;
			c.x[28] = 0;
			c.x[29] = 0;
		};
		for order in 0..2 {
			// both orders: A's entries at base 0 then B's, and the reverse
			let groups = match order {
				0 => vec![a.clone(), b.clone()],
				_ => vec![b.clone(), a.clone()],
			};
			let (bytes, bases) = jit::emit_regions(&groups, &lay).expect("pack emits");
			for &(g, i, retired, exit) in expect.iter() {
				let (blocks, base) = match (g, order) {
					(0, 0) => (&a, bases[0]),
					(0, _) => (&a, bases[1]),
					(_, 0) => (&b, bases[1]),
					_ => (&b, bases[0]),
				};
				let mut cpu = paged_cpu(5);
				setup(&mut cpu);
				cpu.update_pc(blocks[i].0 + bias);
				let mut m = chunked_mem(&engine, &cpu, bias);
				let what = format!("order {} group {} block {:#x}", order, g, blocks[i].0);
				let rw = m.call_region(&engine, &bytes, 1000, base + i as u32);
				assert_eq!((rw, m.get64(STATE + S_PC)), (retired, bias + exit), "{}", what);
				// nothing of the other group ran
				let other = if g == 0 { 29 } else { 28 };
				assert_eq!(m.get64(s_x(other)), 0, "{}: x{} written", what, other);
				// and that is exactly what the interpreter does with this group's blocks alone
				let rt: Vec<(u64, Vec<BlockOp>)> = blocks.iter().map(|b| (b.0 + bias, b.1.clone())).collect();
				let ri = region_ref(&mut cpu, &rt, 1000, None);
				assert_eq!(ri, rw, "{}", what);
				assert_same(&m, &cpu, &what);
			}
		}
		// control: the same blocks as ONE region do run on into each other
		let mut one = a.clone();
		one.extend(b.iter().cloned());
		one.sort_by_key(|b| b.0);
		let bytes = jit::emit_region(&one, &lay).unwrap();
		let mut cpu = paged_cpu(5);
		setup(&mut cpu);
		cpu.update_pc(0x1300 + bias);
		let mut m = chunked_mem(&engine, &cpu, bias);
		let at = one.iter().position(|b| b.0 == 0x1300).unwrap() as u32;
		let rw = m.call_region(&engine, &bytes, 1000, at);
		assert!(rw > 2 && m.get64(s_x(29)) != 0, "one region: the fallthrough continues in-region ({} retired)", rw);
	}

	/// The production import shape: shared memory64 with a declared
	/// maximum. Instantiates against a shared memory and runs.
	#[test]
	fn shared_memory64_import_instantiates_and_runs() {
		let engine = engine();
		let mut lay = chunked_layout(true);
		lay.shared = true;
		lay.max_pages = Some(1 << 10);
		lay.tlb = None;
		lay.ram = jit::Ram::Flat { dram_base: 0x8_0000 };
		lay.ctx = 0x100;
		lay.dram_len = 0x1000;
		// state base 0x1000 (x at +0, pc at +0x100)
		let ops = vec![op(HOT_ADDI, 5, 5, 0, 7), op(HOT_LD, 6, 10, 0, 8)];
		let bytes = jit::emit_region(&[(0, ops)], &lay).unwrap();
		let ty = wasmtime::MemoryTypeBuilder::default().memory64(true).shared(true).min(16).max(Some(1 << 10)).build().unwrap();
		let shm = wasmtime::SharedMemory::new(&engine, ty).unwrap();
		let mut store = wasmtime::Store::new(&engine, ());
		let module = wasmtime::Module::new(&engine, &bytes).unwrap();
		let put = |at: u64, v: u64| {
			for (i, b) in v.to_le_bytes().iter().enumerate() {
				unsafe { *shm.data()[at as usize + i].get() = *b };
			}
		};
		put(0x100 + jit::CTX_BASE, 0x1000);
		put(0x100 + jit::CTX_BIAS, DRAM_BASE);
		put(0x1000 + 5 * 8, 35);
		put(0x1000 + 10 * 8, DRAM_BASE + 0x10);
		put(0x8_0000 + 0x18, 0xfeed);
		let inst = wasmtime::Instance::new(&mut store, &module, &[shm.clone().into()]).unwrap();
		let run = inst.get_typed_func::<(i64, i32), i64>(&mut store, "run").unwrap();
		assert_eq!(run.call(&mut store, (100, 0)).unwrap(), 2);
		let get = |at: u64| {
			let mut b = [0u8; 8];
			for i in 0..8 {
				b[i] = unsafe { *shm.data()[at as usize + i].get() };
			}
			u64::from_le_bytes(b)
		};
		assert_eq!(get(0x1000 + 5 * 8), 42);
		assert_eq!(get(0x1000 + 6 * 8), 0xfeed);
		assert_eq!(get(0x1000 + 0x100), DRAM_BASE + 8, "pc = bias + fallthrough");
	}

	/// Store-side bails under chunked RAM, one cause at a time: the module
	/// stops BEFORE the store (pc exact, nothing written) when the chunk is
	/// not owned, the page is marked executable, or the store lands in a
	/// bookkeeping window; and runs it when none applies.
	#[test]
	fn chunked_store_bails_are_exact() {
		let engine = engine();
		let mut cpu = Cpu::new(Box::new(DummyTerminal::new()));
		cpu.get_mut_mmu().init_memory(RAM);
		cpu.x[10] = (DRAM_BASE + 0x1_0100) as i64; // chunk 1
		cpu.x[11] = 0x5555;
		cpu.update_pc(DRAM_BASE);
		let ops = vec![op(HOT_ADDI, 12, 12, 0, 1), op(HOT_SD, 0, 10, 11, 0), op(HOT_ADDI, 13, 13, 0, 1)];
		let mut lay = chunked_layout(true);
		lay.tlb = None; // bare: physical addresses
		let bytes = jit::emit_region(&[(0, ops)], &lay).unwrap();
		let run = |cpu: &Cpu| {
			let mut m = chunked_mem(&engine, cpu, DRAM_BASE);
			let rw = m.call_region(&engine, &bytes, 100, 0);
			(rw, m.get64(STATE + S_PC), m.get64(CHUNKS + 0x1_0100))
		};
		// chunk 1 never written: Zero, no write pointer
		assert_eq!(run(&cpu), (1, DRAM_BASE + 4, 0), "zero chunk: bail before the store");
		cpu.mmu.store_raw(DRAM_BASE + 0x1_0000, 1); // owned now
		assert_eq!(run(&cpu), (3, DRAM_BASE + 12, 0x5555), "owned chunk: the store runs");
		let _ = cpu.mmu.share_ram();
		assert_eq!(run(&cpu).0, 1, "shared chunk: bail (copy-on-write is the interpreter's)");
		cpu.mmu.store_raw(DRAM_BASE + 0x1_0000, 1);
		assert!(cpu.mmu.mark_exec_page(DRAM_BASE + 0x1_0100));
		assert_eq!(run(&cpu).0, 1, "executable page: bail");
		cpu.mmu.store_raw(DRAM_BASE + 0x2_0000, 1); // a store to an unmarked page...
		let _ = cpu.mmu.store_doubleword(DRAM_BASE + 0x1_0000, 0); // ...and one to the marked page bumps the generation
		cpu.x[10] = (DRAM_BASE + STORE_BAIL.0 + 0x10) as i64;
		cpu.mmu.store_raw(DRAM_BASE + STORE_BAIL.0, 1);
		assert_eq!(run(&cpu).0, 1, "bookkeeping window: bail");
		cpu.x[10] = (DRAM_BASE + STORE_BAIL.1) as i64;
		cpu.mmu.store_raw(DRAM_BASE + STORE_BAIL.1, 1);
		assert_eq!(run(&cpu).0, 3, "just past the window: runs");
		cpu.x[10] = (DRAM_BASE + RAM) as i64;
		assert_eq!(run(&cpu).0, 1, "past the end of DRAM: bail");
		// and a LOAD there (its read pointer would be past the table)
		let lb = jit::emit_region(&[(0, vec![op(HOT_ADDI, 12, 12, 0, 1), op(HOT_LD, 5, 10, 0, 0)])], &lay).unwrap();
		let mut m = chunked_mem(&engine, &cpu, DRAM_BASE);
		m.put64(RDT + (RAM >> 16) * 8, CHUNKS); // a plausible pointer past the table
		assert_eq!(m.call_region(&engine, &lb, 100, 0), 1, "load past the end of DRAM: bail");
	}
}
