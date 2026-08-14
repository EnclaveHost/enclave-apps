//! Prototype for the app-side half of PLATFORM-JIT.md: translate a
//! superblock's predecoded ops into a wasm function and measure what
//! executing it as compiled code buys over interpreting it.
//!
//! Nothing here touches the emulator. It carries its own miniature of the
//! block engine — an op struct and an interpreter loop shaped exactly like
//! `Cpu::exec_block` (per-op: entry load, pc store, dispatch, x0 clear,
//! taken-check, store-generation check) — plus a hand-encoded wasm emitter
//! for the same ops, run under wasmtime-the-crate (dev-dependency; native
//! only). Equivalence is asserted on randomized programs and state before
//! anything is timed.
//!
//!   cargo run --release --example jit-proto
//!
//! The number this exists to produce: the multiplier between "interpret the
//! block" and "call the block as a compiled function", on block shapes like
//! the ones a desktop guest actually runs. The platform ask in
//! PLATFORM-JIT.md estimates 3-8x from template-JIT literature; this
//! replaces the estimate.

extern crate riscv_emu_rust; // unused; keeps the example in-crate
extern crate wasmtime;

use std::time::Instant;

// ---- miniature block IR -------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Addi { rd: u8, rs1: u8, imm: i32 },
    Add { rd: u8, rs1: u8, rs2: u8 },
    Sub { rd: u8, rs1: u8, rs2: u8 },
    And { rd: u8, rs1: u8, rs2: u8 },
    Or { rd: u8, rs1: u8, rs2: u8 },
    Xor { rd: u8, rs1: u8, rs2: u8 },
    Andi { rd: u8, rs1: u8, imm: i32 },
    Slli { rd: u8, rs1: u8, sh: u8 },
    Srli { rd: u8, rs1: u8, sh: u8 },
    Mul { rd: u8, rs1: u8, rs2: u8 },
    Lui { rd: u8, imm: i32 },
    Ld { rd: u8, rs1: u8, imm: i32 },
    Sd { rs1: u8, rs2: u8, imm: i32 },
    Lw { rd: u8, rs1: u8, imm: i32 },
    Sw { rs1: u8, rs2: u8, imm: i32 },
    // conditional branch, always the last op: pc <- taken/fallthrough
    Bne { rs1: u8, rs2: u8, taken_pc: u64, fall_pc: u64 },
}

// ---- the interpreter tier, shaped like exec_block -----------------------

/// State layout mirrors what the wasm function sees in linear memory:
/// x[32] i64 at byte 0, pc u64 at 256, guest RAM from 4096.
struct MachineState {
    x: [i64; 32],
    pc: u64,
    ram: Vec<u8>,
    code_gen: u32,
}

const RAM_BASE: usize = 4096;

fn interp_block(ops: &[Op], st: &mut MachineState, block_gen: u32) -> u64 {
    let mut retired = 0u64;
    for i in 0..ops.len() {
        // mirror exec_block's per-op costs: op load (indexed), pc advance
        // store, dispatch, x0 clear, and the store-op generation check
        let op = ops[i];
        let next = st.pc.wrapping_add(4);
        st.pc = next;
        let mut is_store = false;
        match op {
            Op::Addi { rd, rs1, imm } => {
                st.x[rd as usize] = st.x[rs1 as usize].wrapping_add(imm as i64)
            }
            Op::Add { rd, rs1, rs2 } => {
                st.x[rd as usize] = st.x[rs1 as usize].wrapping_add(st.x[rs2 as usize])
            }
            Op::Sub { rd, rs1, rs2 } => {
                st.x[rd as usize] = st.x[rs1 as usize].wrapping_sub(st.x[rs2 as usize])
            }
            Op::And { rd, rs1, rs2 } => st.x[rd as usize] = st.x[rs1 as usize] & st.x[rs2 as usize],
            Op::Or { rd, rs1, rs2 } => st.x[rd as usize] = st.x[rs1 as usize] | st.x[rs2 as usize],
            Op::Xor { rd, rs1, rs2 } => st.x[rd as usize] = st.x[rs1 as usize] ^ st.x[rs2 as usize],
            Op::Andi { rd, rs1, imm } => st.x[rd as usize] = st.x[rs1 as usize] & imm as i64,
            Op::Slli { rd, rs1, sh } => st.x[rd as usize] = st.x[rs1 as usize] << sh,
            Op::Srli { rd, rs1, sh } => {
                st.x[rd as usize] = ((st.x[rs1 as usize] as u64) >> sh) as i64
            }
            Op::Mul { rd, rs1, rs2 } => {
                st.x[rd as usize] = st.x[rs1 as usize].wrapping_mul(st.x[rs2 as usize])
            }
            Op::Lui { rd, imm } => st.x[rd as usize] = imm as i64,
            Op::Ld { rd, rs1, imm } => {
                let a = (st.x[rs1 as usize].wrapping_add(imm as i64)) as usize;
                let mut b = [0u8; 8];
                b.copy_from_slice(&st.ram[a..a + 8]);
                st.x[rd as usize] = i64::from_le_bytes(b);
            }
            Op::Sd { rs1, rs2, imm } => {
                let a = (st.x[rs1 as usize].wrapping_add(imm as i64)) as usize;
                st.ram[a..a + 8].copy_from_slice(&st.x[rs2 as usize].to_le_bytes());
                is_store = true;
            }
            Op::Lw { rd, rs1, imm } => {
                let a = (st.x[rs1 as usize].wrapping_add(imm as i64)) as usize;
                let mut b = [0u8; 4];
                b.copy_from_slice(&st.ram[a..a + 4]);
                st.x[rd as usize] = i32::from_le_bytes(b) as i64;
            }
            Op::Sw { rs1, rs2, imm } => {
                let a = (st.x[rs1 as usize].wrapping_add(imm as i64)) as usize;
                st.ram[a..a + 4].copy_from_slice(&(st.x[rs2 as usize] as i32).to_le_bytes());
                is_store = true;
            }
            Op::Bne { rs1, rs2, taken_pc, fall_pc } => {
                st.pc = match st.x[rs1 as usize] != st.x[rs2 as usize] {
                    true => taken_pc,
                    false => fall_pc,
                };
            }
        }
        st.x[0] = 0;
        retired += 1;
        if st.pc != next {
            return retired; // taken branch left the block
        }
        if is_store && st.code_gen != block_gen {
            return retired;
        }
    }
    retired
}

// ---- wasm emitter -------------------------------------------------------

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

/// expression helpers over a body buffer
struct Body(Vec<u8>);
impl Body {
    fn i64_const(&mut self, v: i64) {
        self.0.push(0x42);
        sleb(&mut self.0, v);
    }
    fn i32_const(&mut self, v: i32) {
        self.0.push(0x41);
        sleb(&mut self.0, v as i64);
    }
    /// load x[r] onto the stack (i64.load from byte r*8, aligned 8)
    fn get_x(&mut self, r: u8) {
        if r == 0 {
            self.i64_const(0);
            return;
        }
        self.i32_const(0);
        self.0.extend_from_slice(&[0x29, 3]); // i64.load align=8
        uleb(&mut self.0, (r as u64) * 8);
    }
    /// store the stack top into x[r]; rd==x0 becomes a drop
    fn set_x_prologue(&mut self, r: u8) {
        // address operand must precede the value; caller pattern:
        //   set_x_prologue(rd); <compute value>; set_x_epilogue(rd)
        if r != 0 {
            self.i32_const(0);
        }
    }
    fn set_x_epilogue(&mut self, r: u8) {
        if r == 0 {
            self.0.push(0x1a); // drop
            return;
        }
        self.0.extend_from_slice(&[0x37, 3]); // i64.store align=8
        uleb(&mut self.0, (r as u64) * 8);
    }
    fn set_pc(&mut self, v: u64) {
        self.i32_const(0);
        self.i64_const(v as i64);
        self.0.extend_from_slice(&[0x37, 3]);
        uleb(&mut self.0, 256);
    }
}

/// Emit a complete wasm module: import "env" "mem" memory, export
/// "run" () -> i64 executing `ops` straight-line, returning retired count.
/// Loads/stores address guest RAM at linear offset RAM_BASE + addr.
fn emit_op_full(b: &mut Body, op: Op, i: usize) {
    {
        match op {
            Op::Addi { rd, rs1, imm } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.i64_const(imm as i64);
                b.0.push(0x7c); // i64.add
                b.set_x_epilogue(rd);
            }
            Op::Add { rd, rs1, rs2 } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x7c);
                b.set_x_epilogue(rd);
            }
            Op::Sub { rd, rs1, rs2 } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x7d); // i64.sub
                b.set_x_epilogue(rd);
            }
            Op::And { rd, rs1, rs2 } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x83); // i64.and
                b.set_x_epilogue(rd);
            }
            Op::Or { rd, rs1, rs2 } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x84); // i64.or
                b.set_x_epilogue(rd);
            }
            Op::Xor { rd, rs1, rs2 } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x85); // i64.xor
                b.set_x_epilogue(rd);
            }
            Op::Andi { rd, rs1, imm } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.i64_const(imm as i64);
                b.0.push(0x83);
                b.set_x_epilogue(rd);
            }
            Op::Slli { rd, rs1, sh } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.i64_const(sh as i64);
                b.0.push(0x86); // i64.shl
                b.set_x_epilogue(rd);
            }
            Op::Srli { rd, rs1, sh } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.i64_const(sh as i64);
                b.0.push(0x88); // i64.shr_u
                b.set_x_epilogue(rd);
            }
            Op::Mul { rd, rs1, rs2 } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x7e); // i64.mul
                b.set_x_epilogue(rd);
            }
            Op::Lui { rd, imm } => {
                b.set_x_prologue(rd);
                b.i64_const(imm as i64);
                b.set_x_epilogue(rd);
            }
            Op::Ld { rd, rs1, imm } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.i64_const(imm as i64);
                b.0.push(0x7c); // addr = x[rs1] + imm + RAM_BASE
                b.0.push(0xa7); // i32.wrap_i64
                b.0.extend_from_slice(&[0x29, 0, 0]); // i64.load align=1 off=0
                b.set_x_epilogue(rd);
            }
            Op::Sd { rs1, rs2, imm } => {
                b.get_x(rs1);
                b.i64_const(imm as i64);
                b.0.push(0x7c);
                b.0.push(0xa7);
                b.get_x(rs2);
                b.0.extend_from_slice(&[0x37, 0, 0]); // i64.store
            }
            Op::Lw { rd, rs1, imm } => {
                b.set_x_prologue(rd);
                b.get_x(rs1);
                b.i64_const(imm as i64);
                b.0.push(0x7c);
                b.0.push(0xa7);
                b.0.extend_from_slice(&[0x34, 0, 0]); // i64.load32_s
                b.set_x_epilogue(rd);
            }
            Op::Sw { rs1, rs2, imm } => {
                b.get_x(rs1);
                b.i64_const(imm as i64);
                b.0.push(0x7c);
                b.0.push(0xa7);
                b.get_x(rs2);
                b.0.extend_from_slice(&[0x3e, 0, 0]); // i64.store32
            }
            Op::Bne { rs1, rs2, taken_pc, fall_pc } => {
                // pc = (x[rs1] != x[rs2]) ? taken : fall; return retired
                b.i32_const(0);
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x52); // i64.ne
                b.0.push(0x04); // if
                b.0.push(0x7e); // result i64
                b.i64_const(taken_pc as i64);
                b.0.push(0x05); // else
                b.i64_const(fall_pc as i64);
                b.0.push(0x0b); // end
                b.0.extend_from_slice(&[0x37, 3]); // i64.store align 8
                uleb(&mut b.0, 256);
                b.i64_const(i as i64 + 1);
                b.0.push(0x0f); // return
            }
        }
    }
}

/// straight-line ops only (no Bne)
fn emit_op(b: &mut Body, op: Op) {
    emit_op_full(b, op, 0)
}

fn emit_block_module(ops: &[Op]) -> Vec<u8> {
    let mut b = Body(Vec::new());
    let n = ops.len() as i64;
    for (i, op) in ops.iter().enumerate() {
        emit_op_full(&mut b, *op, i);
    }
    // straight-line exit: pc = fallthrough (last op wrote its own), retired = n
    b.set_pc(0x1000 + 4 * ops.len() as u64);
    b.i64_const(n);
    b.0.push(0x0b); // end of expr
    assemble(b, 0)
}

fn assemble(b: Body, n_locals: u32) -> Vec<u8> {
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0];
    // type section: one type () -> i64
    let mut sec = Vec::new();
    uleb(&mut sec, 1);
    sec.extend_from_slice(&[0x60, 0, 1, 0x7e]);
    m.push(1);
    uleb(&mut m, sec.len() as u64);
    m.extend_from_slice(&sec);
    // import section: env.mem memory {min 2}
    let mut sec = Vec::new();
    uleb(&mut sec, 1);
    uleb(&mut sec, 3);
    sec.extend_from_slice(b"env");
    uleb(&mut sec, 3);
    sec.extend_from_slice(b"mem");
    sec.extend_from_slice(&[0x02, 0x00, 2]); // memory, limits min=2
    m.push(2);
    uleb(&mut m, sec.len() as u64);
    m.extend_from_slice(&sec);
    // function section: one func of type 0
    m.extend_from_slice(&[3, 2, 1, 0]);
    // export section: "run" func 0
    let mut sec = Vec::new();
    uleb(&mut sec, 1);
    uleb(&mut sec, 3);
    sec.extend_from_slice(b"run");
    sec.extend_from_slice(&[0x00, 0]);
    m.push(7);
    uleb(&mut m, sec.len() as u64);
    m.extend_from_slice(&sec);
    // code section: one body
    let mut body = Vec::new();
    match n_locals {
        0 => uleb(&mut body, 0),
        n => {
            uleb(&mut body, 1);
            uleb(&mut body, n as u64);
            body.push(0x7e); // i64 locals
        }
    }
    body.extend_from_slice(&b.0);
    let mut sec = Vec::new();
    uleb(&mut sec, 1);
    uleb(&mut sec, body.len() as u64);
    sec.extend_from_slice(&body);
    m.push(10);
    uleb(&mut m, sec.len() as u64);
    m.extend_from_slice(&sec);
    m
}


/// Emit the ops as a REGION: the trailing Bne turns into an internal
/// br_if back to the loop head, so one call runs the whole loop. This is
/// the shape PLATFORM-JIT.md's translator actually needs — the 1.01x of
/// call-per-block proves block granularity cannot pay for the call
/// boundary.
fn emit_region_module(ops: &[Op]) -> Vec<u8> {
    let mut b = Body(Vec::new());
    // local 0: retired counter (declared in the code section below)
    b.0.push(0x03); // loop
    b.0.push(0x40); // blocktype empty
    let n = ops.len() as i64;
    for op in ops.iter() {
        match *op {
            Op::Bne { rs1, rs2, taken_pc, fall_pc } => {
                // retired += n (whole iteration ran)
                b.0.push(0x20); uleb(&mut b.0, 0); // local.get 0
                b.i64_const(n);
                b.0.push(0x7c);
                b.0.push(0x21); uleb(&mut b.0, 0); // local.set 0
                b.get_x(rs1);
                b.get_x(rs2);
                b.0.push(0x52); // i64.ne
                b.0.push(0x0d); uleb(&mut b.0, 0); // br_if to loop head
                // fallthrough: leave the loop
                let _ = (taken_pc, fall_pc);
            }
            _ => emit_op(&mut b, *op),
        }
    }
    b.0.push(0x0b); // end loop
    // pc = fall_pc of the Bne (loop exited); return retired
    let fall = match ops.last() {
        Some(Op::Bne { fall_pc, .. }) => *fall_pc,
        _ => 0,
    };
    b.set_pc(fall);
    b.0.push(0x20); uleb(&mut b.0, 0); // local.get 0
    b.0.push(0x0b); // end expr
    assemble(b, 1)
}

// ---- harness ------------------------------------------------------------

struct WasmBlock {
    store: wasmtime::Store<()>,
    mem: wasmtime::Memory,
    run: wasmtime::TypedFunc<(), i64>,
}

fn compile_block(engine: &wasmtime::Engine, ops: &[Op]) -> WasmBlock {
    let bytes = emit_block_module(ops);
    let module = wasmtime::Module::new(engine, &bytes).expect("valid module");
    let mut store = wasmtime::Store::new(engine, ());
    let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
    let instance = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
    let run = instance
        .get_typed_func::<(), i64>(&mut store, "run")
        .unwrap();
    WasmBlock { store, mem, run }
}

fn write_state(wb: &mut WasmBlock, st: &MachineState) {
    let data = wb.mem.data_mut(&mut wb.store);
    for r in 0..32 {
        data[r * 8..r * 8 + 8].copy_from_slice(&st.x[r].to_le_bytes());
    }
    data[256..264].copy_from_slice(&st.pc.to_le_bytes());
    data[RAM_BASE..RAM_BASE + st.ram.len() - RAM_BASE]
        .copy_from_slice(&st.ram[RAM_BASE..]);
}

fn read_state(wb: &mut WasmBlock, st: &mut MachineState) {
    let data = wb.mem.data(&wb.store);
    for r in 0..32 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&data[r * 8..r * 8 + 8]);
        st.x[r] = i64::from_le_bytes(b);
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[256..264]);
    st.pc = u64::from_le_bytes(b);
    let n = st.ram.len();
    st.ram[RAM_BASE..].copy_from_slice(&data[RAM_BASE..n]);
}

fn fresh_state(seed: u64) -> MachineState {
    // xorshift-filled registers and RAM so runs are deterministic
    let mut s = seed | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut st = MachineState {
        x: [0; 32],
        pc: 0x1000,
        ram: vec![0; 64 * 1024],
        code_gen: 7,
    };
    for r in 1..32 {
        st.x[r] = next() as i64;
    }
    // keep pointer-ish registers inside RAM so loads/stores stay in bounds
    for r in [10u8, 11, 12, 13] {
        st.x[r as usize] = (RAM_BASE as i64 + ((next() % 32768) as i64 & !7)) - RAM_BASE as i64;
        st.x[r as usize] += 8192; // clear of the register file even after imm
    }
    for i in 0..st.ram.len() {
        st.ram[i] = (next() & 0xff) as u8;
    }
    st
}

fn main() {
    let engine = wasmtime::Engine::default();

    // ---- equivalence on randomized straight-line blocks ----
    let mut checked = 0;
    for seed in 1..200u64 {
        let mut s = seed;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let len = 3 + (next() % 14) as usize;
        let mut ops = Vec::new();
        for _ in 0..len {
            let rd = match (next() % 32) as u8 { r @ 10..=13 => r + 10, r => r };
            let rs1 = (10 + next() % 4) as u8; // pointer-ish for mem ops
            let ra = (next() % 32) as u8;
            let rb = (next() % 32) as u8;
            let imm = ((next() % 4096) as i32 & !7) - 2048 + 2048; // 0..4088, 8-aligned
            ops.push(match next() % 12 {
                0 => Op::Addi { rd, rs1: ra, imm: imm - 1024 },
                1 => Op::Add { rd, rs1: ra, rs2: rb },
                2 => Op::Sub { rd, rs1: ra, rs2: rb },
                3 => Op::And { rd, rs1: ra, rs2: rb },
                4 => Op::Or { rd, rs1: ra, rs2: rb },
                5 => Op::Xor { rd, rs1: ra, rs2: rb },
                6 => Op::Slli { rd, rs1: ra, sh: (next() % 63) as u8 },
                7 => Op::Mul { rd, rs1: ra, rs2: rb },
                8 => Op::Lui { rd, imm: imm - 1024 },
                9 => Op::Ld { rd, rs1, imm },
                10 => Op::Sd { rs1, rs2: rb, imm },
                11 => Op::Lw { rd, rs1, imm },
                _ => unreachable!(),
            });
        }
        let mut sti = fresh_state(seed * 977);
        let mut stw = fresh_state(seed * 977);
        let gen = sti.code_gen;
        let ri = interp_block(&ops, &mut sti, gen);
        let mut wb = compile_block(&engine, &ops);
        write_state(&mut wb, &stw);
        let rw = wb.run.call(&mut wb.store, ()).unwrap() as u64;
        read_state(&mut wb, &mut stw);
        // interpreter advances pc per op; wasm sets the block-exit pc once.
        // Same final pc by construction; compare everything.
        assert_eq!(ri, rw, "retired mismatch seed {seed}");
        assert_eq!(sti.x, stw.x, "registers mismatch seed {seed}");
        assert_eq!(sti.pc, stw.pc, "pc mismatch seed {seed}");
        assert_eq!(sti.ram, stw.ram, "ram mismatch seed {seed}");
        checked += 1;
    }
    println!("equivalence: {checked} randomized blocks identical");

    // ---- the benchmark: a DOOM-ish fixed-point hot loop ----
    // body: load two words, multiply-accumulate, mask, store, bump pointer,
    // loop back while counter != 0 — 11 ops, the shape of a texture fill.
    let body = vec![
        Op::Lw { rd: 5, rs1: 10, imm: 0 },
        Op::Lw { rd: 6, rs1: 10, imm: 4 },
        Op::Mul { rd: 7, rs1: 5, rs2: 6 },
        Op::Srli { rd: 7, rs1: 7, sh: 16 },
        Op::Add { rd: 28, rs1: 28, rs2: 7 },
        Op::Andi { rd: 7, rs1: 7, imm: 0xff },
        Op::Sw { rs1: 10, rs2: 7, imm: 8 },
        Op::Addi { rd: 10, rs1: 10, imm: 16 },
        Op::Andi { rd: 10, rs1: 10, imm: 0x1ff0 },
        Op::Add { rd: 10, rs1: 10, rs2: 31 }, // rebase into RAM (x31 = 8192)
        Op::Addi { rd: 29, rs1: 29, imm: -1 },
        Op::Bne { rs1: 29, rs2: 0, taken_pc: 0x1000, fall_pc: 0x1030 },
    ];
    const ITERS: i64 = 3_000_000;

    // interpreter tier, with the block-dispatch overhead the real engine
    // pays per iteration: a probe (hash + head-load + two compares) mocked
    // by a volatile-ish table read the optimizer cannot delete
    let probe_table = vec![0x1000u64; 0x8000];
    let mut sti = fresh_state(42);
    sti.x[29] = ITERS;
    sti.x[10] = 8192;
    sti.x[31] = 8192;
    let gen = sti.code_gen;
    let t0 = Instant::now();
    let mut retired_i = 0u64;
    loop {
        let slot = ((sti.pc >> 1) as usize) & 0x7fff;
        if std::hint::black_box(probe_table[slot]) != 0x1000 {
            break;
        }
        retired_i += interp_block(&body, &mut sti, gen);
        if sti.pc != 0x1000 {
            break;
        }
    }
    let ti = t0.elapsed();

    // compiled tier: same per-iteration host dispatch (probe + typed call)
    let mut wb = compile_block(&engine, &body);
    let mut stw = fresh_state(42);
    stw.x[29] = ITERS;
    stw.x[10] = 8192;
    stw.x[31] = 8192;
    write_state(&mut wb, &stw);
    let t0 = Instant::now();
    let mut retired_w = 0u64;
    loop {
        // dispatch cost per block, same shape as the interpreter tier
        let slot = ((0x1000u64 >> 1) as usize) & 0x7fff;
        if std::hint::black_box(probe_table[slot]) != 0x1000 {
            break;
        }
        retired_w += wb.run.call(&mut wb.store, ()).unwrap() as u64;
        let pc = {
            let d = wb.mem.data(&wb.store);
            let mut b = [0u8; 8];
            b.copy_from_slice(&d[256..264]);
            u64::from_le_bytes(b)
        };
        if pc != 0x1000 {
            break;
        }
    }
    let tw = t0.elapsed();
    read_state(&mut wb, &mut stw);

    assert_eq!(retired_i, retired_w, "benchmark retired mismatch");
    assert_eq!(sti.x, stw.x, "benchmark register mismatch");
    let mips_i = retired_i as f64 / 1e6 / ti.as_secs_f64();
    let mips_w = retired_w as f64 / 1e6 / tw.as_secs_f64();

    // region tier: the branch is an internal br_if, one call runs the loop
    let bytes = emit_region_module(&body);
    let module = wasmtime::Module::new(&engine, &bytes).expect("valid region module");
    let mut store = wasmtime::Store::new(&engine, ());
    let mem = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(2, None)).unwrap();
    let instance = wasmtime::Instance::new(&mut store, &module, &[mem.into()]).unwrap();
    let run = instance.get_typed_func::<(), i64>(&mut store, "run").unwrap();
    let mut wr = WasmBlock { store, mem, run };
    let mut str_ = fresh_state(42);
    str_.x[29] = ITERS;
    str_.x[10] = 8192;
    str_.x[31] = 8192;
    write_state(&mut wr, &str_);
    let t0 = Instant::now();
    let retired_r = wr.run.call(&mut wr.store, ()).unwrap() as u64;
    let tr = t0.elapsed();
    read_state(&mut wr, &mut str_);
    assert_eq!(retired_i, retired_r, "region retired mismatch");
    assert_eq!(sti.x, str_.x, "region register mismatch");
    assert_eq!(sti.pc, str_.pc, "region pc mismatch");
    let mips_r = retired_r as f64 / 1e6 / tr.as_secs_f64();

    println!(
        "hot loop ({} ops/iter, {} iters):\n  interpreter        {:>8.1} MIPS\n  call-per-block JIT {:>8.1} MIPS  ({:.2}x)\n  region JIT         {:>8.1} MIPS  ({:.2}x)",
        body.len(), ITERS,
        mips_i, mips_w, mips_w / mips_i, mips_r, mips_r / mips_i
    );
}
