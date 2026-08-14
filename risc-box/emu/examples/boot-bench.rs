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
    // --type: bytes queued here are fed to the guest UART as serial input
    input: Rc<RefCell<std::collections::VecDeque<u8>>>,
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
        self.input.borrow_mut().pop_front().unwrap_or(0)
    }
    fn put_input(&mut self, value: u8) {
        self.input.borrow_mut().push_back(value);
    }
    fn get_output(&mut self) -> u8 {
        0
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut budget: u64 = 2_000_000_000;
    let mut ram_mib: u64 = 0; // 0 = emulator default (512 MiB)
    let mut until: Option<String> = None;
    let mut echo = false;
    // --trace-tf FILE: watch the console for a V8 --print-opt-code dump, parse
    // the code's start address + size, then log every executed instruction in
    // that range (pc offset + which registers changed to what) to FILE. This
    // is the execution-truth companion to the static dump: any mis-executed
    // instruction shows up as a register value inconsistent with its inputs.
    let mut type_script: Vec<(u64, String)> = Vec::new();
    let mut xkey_script: Vec<(u64, String)> = Vec::new();
    let mut snap_script: Vec<(u64, String)> = Vec::new();
    let mut xclick_script: Vec<(u64, u32, u32)> = Vec::new();
    let mut xkey_on_script: Vec<(Vec<u8>, String)> = Vec::new();
    let mut trace_file: Option<String> = None;
    let mut trace_limit: u64 = 200_000;
    let mut trace_sub: Option<(u64, u64)> = None;
    let mut trace_call: Option<u64> = None;
    let mut callee_range: Option<(u64, u64)> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--insns" => {
                i += 1;
                budget = args[i].replace('_', "").parse().expect("--insns takes a number");
            }
            "--ram-mib" => {
                // guest DRAM size; the app wires the deployment's ramMiB the
                // same way. The Alpine desktop runs the fleet at 1792.
                i += 1;
                ram_mib = args[i].parse().expect("--ram-mib takes MiB");
            }
            "--until" => {
                i += 1;
                until = Some(args[i].clone());
            }
            "--console" => echo = true,
            "--type" => {
                // SECONDS:COMMAND -- queue COMMAND (plus newline) as serial
                // input once the wall clock passes SECONDS. Repeatable.
                i += 1;
                let (secs, cmd) = args[i].split_once(':').expect("--type SECONDS:COMMAND");
                type_script.push((secs.parse::<u64>().expect("seconds"), format!("{}\n", cmd)));
            }
            "--snap" => {
                // SECONDS:FILE -- once the wall clock passes SECONDS, dump
                // the 1024x768 framebuffer as a binary PPM to FILE.
                i += 1;
                let (secs, path) = args[i].split_once(':').expect("--snap SECONDS:FILE");
                snap_script.push((secs.parse::<u64>().expect("seconds"), path.to_string()));
            }
            "--xclick" => {
                // SECONDS:X:Y -- left-click at screen pixel (X,Y) via the
                // absolute virtio-input pointer. Use before --xkey: fluxbox
                // assigns initial focus nondeterministically, and a click
                // makes the target window focused for certain.
                i += 1;
                let parts: Vec<&str> = args[i].split(':').collect();
                xclick_script.push((
                    parts[0].parse::<u64>().expect("seconds"),
                    parts[1].parse::<u32>().expect("x"),
                    parts[2].parse::<u32>().expect("y"),
                ));
            }
            "--xkey-on" => {
                // MARKER:TEXT -- inject TEXT as X keyboard events when MARKER
                // appears on the guest console. Wall-clock --xkey races the X
                // session's keyboard subdevice, which arrives seconds after
                // the desktop LOOKS ready (later still when Xorg retried);
                // pair this with a serial poll that prints the marker once
                // /var/log/Xorg.0.log shows 'type: KEYBOARD'.
                i += 1;
                let (marker, txt) = args[i].split_once(':').expect("--xkey-on MARKER:TEXT");
                xkey_on_script.push((marker.as_bytes().to_vec(), txt.to_string()));
            }
            "--xkey" => {
                // SECONDS:TEXT -- once the wall clock passes SECONDS, inject
                // TEXT into the guest as virtio-input KEYBOARD events (press,
                // release, SYN per character), the same path the app's
                // browser input uses. Reaches an X session, which serial
                // --type cannot. "\n" for Enter is written as "$".
                i += 1;
                let (secs, txt) = args[i].split_once(':').expect("--xkey SECONDS:TEXT");
                xkey_script.push((secs.parse::<u64>().expect("seconds"), txt.to_string()));
            }
            "--trace-tf" => {
                i += 1;
                trace_file = Some(args[i].clone());
            }
            "--trace-limit" => {
                i += 1;
                trace_limit = args[i].replace('_', "").parse().expect("--trace-limit takes a number");
            }
            "--trace-call" => {
                // OFFSET (hex, relative to the armed code start) of a jalr
                // call site: on reaching it, read t6 (the target), dump the
                // callee's first 8 KiB of code words, then trace the callee's
                // execution (entry snapshots + reg diffs) instead of the
                // caller range.
                i += 1;
                trace_call = Some(u64::from_str_radix(args[i].trim_start_matches("+"), 16).expect("hex off"));
            }
            "--trace-sub" => {
                // OFFLO:OFFHI (hex, relative to the armed code start): only log
                // instructions inside this sub-window, with a full register
                // snapshot each time execution ENTERS it, so a verifier has
                // complete state per visit.
                i += 1;
                let parts: Vec<&str> = args[i].split(':').collect();
                trace_sub = Some((
                    u64::from_str_radix(parts[0].trim_start_matches("+"), 16).expect("hex off"),
                    u64::from_str_radix(parts[1].trim_start_matches("+"), 16).expect("hex off"),
                ));
            }
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
    let ser_in = Rc::new(RefCell::new(std::collections::VecDeque::new()));
    let mut emu = Emulator::new(Box::new(BenchTerminal {
        out: console.clone(),
        input: ser_in.clone(),
        echo: match echo {
            true => Some(std::io::BufWriter::new(std::io::stdout())),
            false => None,
        },
    }));
    if ram_mib > 0 {
        emu.setup_ram_bytes(ram_mib * 1024 * 1024);
    }
    emu.setup_program(kernel);
    emu.setup_filesystem(fs);

    // Batch size matches the app's TICK_BATCH so the loop overhead outside
    // tick() is the same shape as production.
    const BATCH: u64 = 400_000;
    let start = Instant::now();
    let mut done: u64 = 0;
    let mut last_fbw: u64 = 0;
    let mut window = Instant::now();
    let mut window_insns: u64 = 0;

    // --trace-tf state
    let mut trace_range: Option<(u64, u64)> = None; // armed once the dump is seen
    let mut trace_out: Option<std::io::BufWriter<std::fs::File>> = None;
    let mut traced: u64 = 0;
    let mut was_in = false;
    let mut skip_visit = false;
    let mut prev_regs = [0i64; 32];

    while done < budget {
        match trace_range {
            Some((lo, hi)) if traced < trace_limit => {
                // armed: single-step so every in-range instruction is observed
                use std::io::Write;
                for _ in 0..BATCH {
                    let pc = emu.get_cpu().read_pc();
                    // callee-tracing mode: arm the callee range at the call site.
                    // The site is found by its unique signature rather than a
                    // fixed offset (TF layouts drift between compilations):
                    // `jalr ra, 0(t6)` immediately followed by `srai a4, a0, 32`.
                    if let (Some(_), None) = (trace_call, callee_range) {
                        let in_range = pc >= lo && pc < hi;
                        let sig = in_range
                            && emu.get_mut_cpu().get_mut_mmu().load_word(pc).map(|w| w == 0x000f80e7).unwrap_or(false)
                            && emu.get_mut_cpu().get_mut_mmu().load_word(pc + 4).map(|w| w == 0x42055713).unwrap_or(false);
                        if sig {
                            let t6 = emu.get_cpu().read_register(31) as u64;
                            callee_range = Some((t6, t6 + 0x2000));
                            eprintln!("CALLEE_ARMED {:#x}", t6);
                            // dump the callee's code words for the verifier
                            if let Some(w) = trace_out.as_mut() {
                                use std::io::Write;
                                for a in (t6..t6 + 0x2000).step_by(8) {
                                    match emu.get_mut_cpu().get_mut_mmu().load_doubleword(a) {
                                        Ok(d) => { let _ = w.write_all(format!("CODE {:x} {:016x}\n", a - t6, d).as_bytes()); }
                                        Err(_) => break,
                                    }
                                }
                                let _ = w.flush();
                            }
                        }
                    }
                    let (slo, shi) = match (callee_range, trace_call) {
                        (Some((a, b)), _) => (a, b),
                        (None, Some(_)) => (u64::MAX, u64::MAX), // callee not armed yet: trace nothing
                        _ => match trace_sub {
                            Some((a, b)) => (lo + a, lo + b),
                            None => (lo, hi),
                        },
                    };
                    let mut hit = pc >= slo && pc < shi;
                    if hit && !was_in && callee_range.is_some() {
                        // only trace builtin visits CALLED FROM the TF code
                        let ra = emu.get_cpu().read_register(1) as u64;
                        if !(ra >= lo && ra < hi) {
                            skip_visit = true;
                        } else {
                            skip_visit = false;
                        }
                    }
                    if callee_range.is_some() && skip_visit {
                        hit = false;
                    }
                    if hit && !was_in {
                        // full snapshot on entry: the verifier gets complete state
                        if let Some(w) = trace_out.as_mut() {
                            use std::io::Write;
                            let mut line = format!("={:x}", pc - slo);
                            for r in 1..32 {
                                line.push_str(&format!(" x{}={:x}", r, emu.get_cpu().read_register(r as u8) as u64));
                            }
                            line.push('\n');
                            let _ = w.write_all(line.as_bytes());
                        }
                    }
                    was_in = hit;
                    if hit {
                        for r in 1..32 {
                            prev_regs[r] = emu.get_cpu().read_register(r as u8);
                        }
                    }
                    emu.tick();
                    if hit {
                        if let Some(w) = trace_out.as_mut() {
                            let mut line = format!("+{:x}", pc - slo);
                            for r in 1..32 {
                                let v = emu.get_cpu().read_register(r as u8);
                                if v != prev_regs[r] {
                                    line.push_str(&format!(" x{}={:x}", r, v as u64));
                                }
                            }
                            line.push('\n');
                            let _ = w.write_all(line.as_bytes());
                        }
                        traced += 1;
                        if traced == trace_limit {
                            if let Some(w) = trace_out.as_mut() { let _ = w.flush(); }
                            eprintln!("TRACE_CAPTURED {} lines", traced);
                        }
                    }
                }
            }
            _ => {
                // batched entry point: same instruction count, loop overhead
                // amortized inside the emulator (mirrors the app's loop)
                emu.run_n(BATCH);
                // A WFI-parked guest consumes its batch without executing, so
                // an idle guest would otherwise burn the whole --insns budget
                // in moments of wall time and break the --type/--until script.
                // Pace idle batches to roughly the pre-fast-forward idle rate
                // (~400 MIPS) so script timings stay comparable.
                if emu.get_cpu().is_idle() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        if trace_file.is_some() && trace_range.is_none() {
            // Not armed yet: watch the console for the dump header, then the
            // first instruction line:  "Instructions (size = N)\n0xADDR ..."
            let buf = console.borrow();
            if let Some(pos) = find_sub(&buf, b"Instructions (size = ") {
                let tail = &buf[pos..];
                if let Some((size, rest_off)) = parse_usize_after(tail, b"Instructions (size = ") {
                    if let Some(addr) = parse_hex_line_start(&tail[rest_off..]) {
                        drop(buf);
                        let f = std::fs::File::create(trace_file.as_ref().unwrap()).expect("trace file");
                        trace_out = Some(std::io::BufWriter::new(f));
                        trace_range = Some((addr, addr + size as u64));
                        eprintln!("TRACE_ARMED {:#x}..{:#x}", addr, addr + size as u64);
                    }
                }
            }
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
            let elapsed_s = start.elapsed().as_secs();
            type_script.retain(|(at, cmd)| {
                if *at <= elapsed_s {
                    for b in cmd.bytes() {
                        ser_in.borrow_mut().push_back(b);
                    }
                    eprintln!("TYPED@{}s: {}", elapsed_s, cmd.trim_end());
                    false
                } else {
                    true
                }
            });
            let mut clicks: Vec<(u32, u32)> = Vec::new();
            xclick_script.retain(|(at, x, y)| {
                if *at <= elapsed_s {
                    clicks.push((*x, *y));
                    false
                } else {
                    true
                }
            });
            for (x, y) in clicks {
                let max = riscv_emu_rust::Emulator::input_abs_max() as u64;
                let ax = (x as u64 * max / 1023) as u32;
                let ay = (y as u64 * max / 767) as u32;
                emu.push_input_event(3, 0, ax); // EV_ABS ABS_X
                emu.push_input_event(3, 1, ay); // EV_ABS ABS_Y
                emu.push_input_event(0, 0, 0);
                emu.push_input_event(1, 0x110, 1); // BTN_LEFT down
                emu.push_input_event(0, 0, 0);
                emu.push_input_event(1, 0x110, 0); // BTN_LEFT up
                emu.push_input_event(0, 0, 0);
                eprintln!("XCLICKED@{}s: {},{}", elapsed_s, x, y);
            }
            let mut fired: Vec<String> = Vec::new();
            xkey_script.retain(|(at, txt)| {
                if *at <= elapsed_s {
                    fired.push(txt.clone());
                    false
                } else {
                    true
                }
            });
            {
                let buf = console.borrow();
                xkey_on_script.retain(|(marker, txt)| {
                    if buf.windows(marker.len()).any(|w| w == &marker[..]) {
                        fired.push(txt.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            let mut snaps: Vec<String> = Vec::new();
            snap_script.retain(|(at, path)| {
                if *at <= elapsed_s {
                    snaps.push(path.clone());
                    false
                } else {
                    true
                }
            });
            for path in snaps {
                // simplefb: 1024x768 XRGB8888 at 0x87e00000, 4096-byte stride
                let (w, h, stride) = (1024usize, 768usize, 4096usize);
                let mut fb = vec![0u8; stride * h];
                emu.read_physical_range(0x87e0_0000, &mut fb);
                let mut ppm = format!("P6\n{} {}\n255\n", w, h).into_bytes();
                for y in 0..h {
                    for x in 0..w {
                        let o = y * stride + x * 4;
                        ppm.push(fb[o + 2]); // R (XRGB little-endian)
                        ppm.push(fb[o + 1]); // G
                        ppm.push(fb[o]); // B
                    }
                }
                std::fs::write(&path, &ppm).expect("snap write");
                eprintln!("SNAPPED@{}s: {}", elapsed_s, path);
            }
            for txt in fired {
                for ch in txt.chars() {
                    if let Some((code, shift)) = linux_keycode(ch) {
                        if shift {
                            emu.push_input_event(1, 42, 1); // KEY_LEFTSHIFT down
                            emu.push_input_event(0, 0, 0);
                        }
                        emu.push_input_event(1, code, 1);
                        emu.push_input_event(0, 0, 0);
                        emu.push_input_event(1, code, 0);
                        emu.push_input_event(0, 0, 0);
                        if shift {
                            emu.push_input_event(1, 42, 0);
                            emu.push_input_event(0, 0, 0);
                        }
                    }
                }
                eprintln!("XKEYED@{}s: {}", elapsed_s, txt);
            }
            let mips = window_insns as f64 / 1e6 / window.elapsed().as_secs_f64();
            // debug aid: framebuffer store rate + a two-pixel probe (origin and
            // center) so display transitions land in the same log as the rate.
            let fbw = emu.fb_writes();
            let dfbw = fbw.wrapping_sub(last_fbw);
            last_fbw = fbw;
            let mut px = [0u8; 4];
            emu.read_physical_range(0x87e0_0000, &mut px);
            let mut cx = [0u8; 4];
            emu.read_physical_range(0x87e0_0000 + (384 * 4096 + 512 * 4) as u64, &mut cx);
            // corner probes OUTSIDE a 640x480 window at origin: bottom-right + right-mid
            let mut br = [0u8; 4];
            emu.read_physical_range(0x87e0_0000 + (740 * 4096 + 1000 * 4) as u64, &mut br);
            let mut rm = [0u8; 4];
            emu.read_physical_range(0x87e0_0000 + (300 * 4096 + 900 * 4) as u64, &mut rm);
            eprintln!(
                "  {:>6.1}s  {:>6.0}M insns  {:>7.1} MIPS  fbw+{} px={:02x}{:02x}{:02x} cx={:02x}{:02x}{:02x} rm={:02x}{:02x}{:02x} br={:02x}{:02x}{:02x}",
                start.elapsed().as_secs_f64(),
                done as f64 / 1e6,
                mips,
                dfbw,
                px[2], px[1], px[0],
                cx[2], cx[1], cx[0],
                rm[2], rm[1], rm[0],
                br[2], br[1], br[0]
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


/// ASCII -> (Linux input keycode, needs shift). Enough for typing shell
/// commands into an X terminal; '$' stands in for Enter so --xkey values
/// survive shell quoting.
fn linux_keycode(ch: char) -> Option<(u16, bool)> {
    let (code, shift) = match ch {
        '1' => (2, false), '2' => (3, false), '3' => (4, false), '4' => (5, false),
        '5' => (6, false), '6' => (7, false), '7' => (8, false), '8' => (9, false),
        '9' => (10, false), '0' => (11, false),
        'q' => (16, false), 'w' => (17, false), 'e' => (18, false), 'r' => (19, false),
        't' => (20, false), 'y' => (21, false), 'u' => (22, false), 'i' => (23, false),
        'o' => (24, false), 'p' => (25, false),
        'a' => (30, false), 's' => (31, false), 'd' => (32, false), 'f' => (33, false),
        'g' => (34, false), 'h' => (35, false), 'j' => (36, false), 'k' => (37, false),
        'l' => (38, false),
        'z' => (44, false), 'x' => (45, false), 'c' => (46, false), 'v' => (47, false),
        'b' => (48, false), 'n' => (49, false), 'm' => (50, false),
        ' ' => (57, false), '-' => (12, false), '=' => (13, false),
        '.' => (52, false), '/' => (53, false), ';' => (39, false),
        '$' => (28, false), // Enter
        '_' => (12, true), '&' => (8, true), '|' => (43, true),
        _ => return None,
    };
    Some((code, shift))
}

// --trace-tf helpers -----------------------------------------------------

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parses the decimal number that follows `prefix` inside `tail` (which must
/// start with `prefix`); returns (number, offset just past the number).
fn parse_usize_after(tail: &[u8], prefix: &[u8]) -> Option<(usize, usize)> {
    let mut i = prefix.len();
    let mut n: usize = 0;
    let mut any = false;
    while i < tail.len() && tail[i].is_ascii_digit() {
        n = n * 10 + (tail[i] - b'0') as usize;
        i += 1;
        any = true;
    }
    match any {
        true => Some((n, i)),
        false => None,
    }
}

/// Finds the first "0x<hex>" at a line start in `tail` and parses it — the
/// first disassembly line's instruction address.
fn parse_hex_line_start(tail: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i + 2 < tail.len() {
        // seek to just after a newline
        while i < tail.len() && tail[i] != b'\n' {
            i += 1;
        }
        i += 1;
        if i + 2 >= tail.len() {
            return None;
        }
        if tail[i] == b'0' && tail[i + 1] == b'x' {
            let mut j = i + 2;
            let mut v: u64 = 0;
            let mut any = false;
            while j < tail.len() && tail[j].is_ascii_hexdigit() {
                v = v * 16
                    + match tail[j] {
                        b'0'..=b'9' => (tail[j] - b'0') as u64,
                        b'a'..=b'f' => (tail[j] - b'a') as u64 + 10,
                        _ => (tail[j] - b'A') as u64 + 10,
                    };
                j += 1;
                any = true;
            }
            if any {
                return Some(v);
            }
        }
    }
    None
}
