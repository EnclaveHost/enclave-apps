//! Emulator throughput benchmark: boot a real guest and measure MIPS.
//!
//! Performance work on the interpreter needs a repeatable number, and the
//! deployed app's `/status` MIPS is not it — that figure is measured on the
//! wasm build while the same thread is also serving HTTP, scanning the
//! framebuffer and encoding video, so it moves with what the browser is doing.
//! This runs the emulator and nothing else, natively, so a change to the
//! instruction loop can be attributed to the instruction loop.
//!
//!   cargo run --release --example boot-bench -- <fw_payload.elf> <rootfs.ext2>
//!
//! Options:
//!   --insns N     stop after N instructions (default 2_000_000_000)
//!   --until STR   stop when STR appears on the guest console, and report the
//!                 instructions and seconds it took to get there. This is the
//!                 metric that matters for boot time — "instructions to reach
//!                 the login prompt" is a property of the guest, so it holds
//!                 still while the emulator underneath it gets faster.
//!   --console     echo the guest console to stdout as it boots
//!
//! It is deliberately an example rather than a test: it needs guest images,
//! runs for minutes, and is a measuring tool, not an assertion.

// The vendored crate is edition 2015 (upstream's Cargo.toml declared no
// edition), so the example has to name the crate explicitly.
extern crate riscv_emu_rust;

use riscv_emu_rust::terminal::Terminal;
use riscv_emu_rust::Emulator;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

/// Console sink that keeps the tail of the output for `--until` matching.
/// `DefaultTerminal` drains with `Vec::remove(0)`, which is O(n) per byte and
/// would show up in the profile as emulator time; this only ever appends.
/// The buffer is shared with main so the marker can be checked between
/// batches without reaching back through the emulator.
struct BenchTerminal {
    out: Rc<RefCell<Vec<u8>>>,
    // Buffered, and flushed a line at a time. Echoing straight to stdout costs
    // a syscall per character, which is slow enough to halve the measured MIPS
    // and make `--console` runs look like they stalled.
    echo: Option<std::io::BufWriter<std::io::Stdout>>,
}

impl Terminal for BenchTerminal {
    fn put_byte(&mut self, value: u8) {
        if let Some(w) = self.echo.as_mut() {
            use std::io::Write;
            let _ = w.write_all(&[value]);
            if value == b'\n' {
                let _ = w.flush();
            }
        }
        let mut out = self.out.borrow_mut();
        out.push(value);
        // bound the retained tail; `--until` only ever looks at the end
        if out.len() > 1 << 20 {
            out.drain(..1 << 19);
        }
    }
    fn get_input(&mut self) -> u8 {
        0
    }
    fn put_input(&mut self, _value: u8) {}
    fn get_output(&mut self) -> u8 {
        0
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut budget: u64 = 2_000_000_000;
    let mut until: Option<String> = None;
    let mut echo = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--insns" => {
                i += 1;
                budget = args[i].replace('_', "").parse().expect("--insns takes a number");
            }
            "--until" => {
                i += 1;
                until = Some(args[i].clone());
            }
            "--console" => echo = true,
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    if positional.len() < 2 {
        eprintln!("usage: boot-bench <fw_payload.elf> <rootfs.ext2> [--insns N] [--until STR] [--console]");
        std::process::exit(2);
    }

    let kernel = std::fs::read(&positional[0]).expect("read kernel");
    let fs = std::fs::read(&positional[1]).expect("read rootfs");
    eprintln!(
        "boot-bench: kernel {} bytes, rootfs {} bytes",
        kernel.len(),
        fs.len()
    );

    // Mirror the app's boot exactly (src/main.rs `boot`), minus the network:
    // no host sockets, so the measurement has no external dependency.
    let console = Rc::new(RefCell::new(Vec::new()));
    let mut emu = Emulator::new(Box::new(BenchTerminal {
        out: console.clone(),
        echo: match echo {
            true => Some(std::io::BufWriter::new(std::io::stdout())),
            false => None,
        },
    }));
    emu.setup_program(kernel);
    emu.setup_filesystem(fs);

    // Batch size matches the app's TICK_BATCH so the loop overhead outside
    // tick() is the same shape as production.
    const BATCH: u64 = 400_000;
    let start = Instant::now();
    let mut done: u64 = 0;
    let mut window = Instant::now();
    let mut window_insns: u64 = 0;

    while done < budget {
        for _ in 0..BATCH {
            emu.tick();
        }
        done += BATCH;
        window_insns += BATCH;

        if let Some(marker) = &until {
            // The marker is ASCII, so a byte-window search finds it wherever
            // it sits in the retained tail.
            let buf = console.borrow();
            if buf
                .windows(marker.len())
                .any(|w| w == marker.as_bytes())
            {
                let secs = start.elapsed().as_secs_f64();
                println!(
                    "REACHED {:?} after {:.0}M instructions in {:.2}s = {:.1} MIPS",
                    marker,
                    done as f64 / 1e6,
                    secs,
                    done as f64 / 1e6 / secs
                );
                return;
            }
        }

        if window.elapsed().as_secs_f64() >= 2.0 {
            let mips = window_insns as f64 / 1e6 / window.elapsed().as_secs_f64();
            eprintln!(
                "  {:>6.1}s  {:>6.0}M insns  {:>7.1} MIPS",
                start.elapsed().as_secs_f64(),
                done as f64 / 1e6,
                mips
            );
            window = Instant::now();
            window_insns = 0;
        }
    }

    let secs = start.elapsed().as_secs_f64();
    println!(
        "TOTAL {:.0}M instructions in {:.2}s = {:.1} MIPS",
        done as f64 / 1e6,
        secs,
        done as f64 / 1e6 / secs
    );
}
