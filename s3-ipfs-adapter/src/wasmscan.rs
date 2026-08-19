//! Streaming WebAssembly classifier for /add-wasm, the Rust port of the nan
//! gateway's Tier-1 validation (ipfs-add-gateway.py: preamble_error,
//! module_mem64, component_contract). The Python original held the whole
//! upload in memory; this one is fed the body as it streams, because a 2 GiB
//! component cannot be buffered in a wasm32 guest. What it keeps is bounded:
//! the 8-byte preamble, a rolling 24-byte window for the marker scans, and
//! the payload of the few TOP-LEVEL sections the classifier actually reads
//! (component export/import, core-module memory), each under a hard cap.
//!
//! Contract parity notes (keep in lockstep with the runner's
//! wasm_manager._check_component / _component_contract, the launch
//! authority — this copy only REPORTS so publish clients can stamp `wasi`
//! into the version config for claim routing):
//!   - layer 0 (core module) is refused UNLESS it declares a 64-bit linear
//!     memory (the wasm64 COMPUTE-guest carve-out);
//!   - layer 1 (component) is accepted; its deciding EXPORT names the wasi
//!     world, scanned from length-prefixed `wasi:*` strings in the top-level
//!     export section only — nested core modules are never opened;
//!   - `[thread-` and `[set-spawn-indirect]` are raw whole-stream substring
//!     scans (same doctrine as the Python);
//!   - classification is NEVER a refusal: anything unparseable reports
//!     wasi=null and uploads exactly as before.

/// Section payloads the classifier reads are small (an export section is a
/// few KiB). A section that claims more than this is skipped, not stored:
/// classification degrades to null, never to unbounded memory.
const KEEP_CAP: usize = 4 * 1024 * 1024;

const MARK_THREAD: &[u8] = b"[thread-";
const MARK_SET: &[u8] = b"[set-spawn-indirect]";

#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub wasi: Option<&'static str>, // "0.2" | "0.3"
    pub world: Option<String>,
    pub threads: bool,
    pub set: bool,
    pub mem64: bool,
}

pub struct WasmScan {
    total: u64,
    head: Vec<u8>, // first 8 bytes
    layer: Option<u16>,
    // rolling marker scans
    tail: Vec<u8>, // last max(marker)-1 bytes of the previous feed
    threads: bool,
    set: bool,
    // top-level section walker
    walk: Walk,
    export_payload: Vec<u8>, // component export section (id 11)
    memory_payload: Vec<u8>, // core-module memory section (id 5)
    walk_dead: bool,         // structure unparseable: classify as null
}

enum Walk {
    NeedId,
    NeedSize { id: u8, varint: Vec<u8> },
    InSection { id: u8, remaining: u64 },
}

impl WasmScan {
    pub fn new() -> WasmScan {
        WasmScan {
            total: 0,
            head: Vec::with_capacity(8),
            layer: None,
            tail: Vec::new(),
            threads: false,
            set: false,
            walk: Walk::NeedId,
            export_payload: Vec::new(),
            memory_payload: Vec::new(),
            walk_dead: false,
        }
    }

    /// Feed the next run of body bytes. Returns Err(reason) only for the
    /// Tier-1 preamble refusals, which are decidable at the first 8 bytes;
    /// everything later is classification, never refusal.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), String> {
        let mut data = data;
        // Preamble: collect exactly 8 bytes before anything else.
        if self.head.len() < 8 {
            let take = (8 - self.head.len()).min(data.len());
            self.head.extend_from_slice(&data[..take]);
            data = &data[take..];
            self.total += take as u64;
            if self.head.len() == 8 {
                if &self.head[0..4] != b"\x00asm" {
                    return Err("not a WebAssembly file (missing the \\0asm magic bytes)".into());
                }
                let layer = u16::from(self.head[6]) | (u16::from(self.head[7]) << 8);
                if layer != 0 && layer != 1 {
                    return Err(format!(
                        "unrecognized wasm layer {layer} - expected a component"
                    ));
                }
                self.layer = Some(layer);
            }
            if data.is_empty() {
                return Ok(());
            }
        }
        self.total += data.len() as u64;

        // Marker scans across the feed boundary.
        if !self.threads || !self.set {
            let mut window = std::mem::take(&mut self.tail);
            window.extend_from_slice(data);
            if !self.threads && contains(&window, MARK_THREAD) {
                self.threads = true;
            }
            if !self.set && contains(&window, MARK_SET) {
                self.set = true;
            }
            let keep = (MARK_SET.len() - 1).min(window.len());
            self.tail = window[window.len() - keep..].to_vec();
        }

        // Top-level section walk.
        if !self.walk_dead {
            self.walk_bytes(data);
        }
        Ok(())
    }

    fn walk_bytes(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            match &mut self.walk {
                Walk::NeedId => {
                    let id = data[0];
                    data = &data[1..];
                    self.walk = Walk::NeedSize { id, varint: Vec::new() };
                }
                Walk::NeedSize { id, varint } => {
                    let id = *id;
                    let mut done = None;
                    while let Some((&b, rest)) = data.split_first() {
                        data = rest;
                        varint.push(b);
                        if varint.len() > 10 {
                            self.walk_dead = true;
                            return;
                        }
                        if b & 0x80 == 0 {
                            let mut v: u64 = 0;
                            for (i, &vb) in varint.iter().enumerate() {
                                v |= u64::from(vb & 0x7f) << (7 * i);
                            }
                            done = Some(v);
                            break;
                        }
                    }
                    if let Some(size) = done {
                        self.walk = Walk::InSection { id, remaining: size };
                    }
                }
                Walk::InSection { id, remaining } => {
                    let take = (*remaining).min(data.len() as u64) as usize;
                    let want = match (self.layer, *id) {
                        (Some(1), 11) => Some(&mut self.export_payload),
                        (Some(0), 5) => Some(&mut self.memory_payload),
                        _ => None,
                    };
                    if let Some(buf) = want {
                        if buf.len() + take <= KEEP_CAP {
                            buf.extend_from_slice(&data[..take]);
                        } else {
                            buf.clear(); // over cap: classify from nothing
                        }
                    }
                    *remaining -= take as u64;
                    data = &data[take..];
                    if *remaining == 0 {
                        self.walk = Walk::NeedId;
                    }
                }
            }
        }
    }

    /// Tier-1 verdict once the whole body has been fed. Err = refusal with
    /// the same messages the Python gateway answers.
    pub fn accept_error(&self) -> Option<String> {
        if self.head.len() < 8 {
            return Some("too small to be a WebAssembly module".into());
        }
        match self.layer {
            Some(1) => None,
            Some(0) => {
                if self.mem64() {
                    None
                } else {
                    Some("this is a core wasm module, but Enclave runs wasi:http components".into())
                }
            }
            _ => Some("not a WebAssembly file (missing the \\0asm magic bytes)".into()),
        }
    }

    fn mem64(&self) -> bool {
        // First memory's limits flags carry the memory64 bit (0x04).
        let b = &self.memory_payload;
        let (count, p) = match uleb(b, 0) {
            Some(v) => v,
            None => return false,
        };
        if count == 0 {
            return false;
        }
        match uleb(b, p) {
            Some((flags, _)) => flags & 0x04 != 0,
            None => false,
        }
    }

    pub fn verdict(&self) -> Verdict {
        let mut v = Verdict {
            wasi: None,
            world: None,
            threads: self.threads,
            set: self.set,
            mem64: self.layer == Some(0) && self.mem64(),
        };
        if self.layer != Some(1) || self.walk_dead {
            return v;
        }
        let exports = wasi_names(&self.export_payload);
        for (prefix, ver) in [
            ("wasi:http/handler@0.3.", "0.3"),
            ("wasi:http/incoming-handler@0.2.", "0.2"),
            ("wasi:cli/run@0.3.", "0.3"),
            ("wasi:cli/run@0.2.", "0.2"),
        ] {
            let mut hit: Vec<&String> = exports.iter().filter(|e| e.starts_with(prefix)).collect();
            hit.sort();
            if let Some(first) = hit.first() {
                v.wasi = Some(ver);
                v.world = Some((*first).clone());
                return v;
            }
        }
        v
    }
}

/// Length-prefixed `wasi:*` names inside a section payload, the Python
/// backtracking scan ported: find "wasi:", try the 1..5 bytes before it as a
/// uleb length whose value spans a name of the allowed charset.
fn wasi_names(payload: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = find(payload, b"wasi:", i) {
        for back in 1..6 {
            if p < back {
                break;
            }
            let Some((ln, q)) = uleb(payload, p - back) else { continue };
            if q == p && p + (ln as usize) <= payload.len() {
                let s = &payload[p..p + ln as usize];
                if !s.is_empty() && s.iter().all(|&b| name_char(b)) {
                    if let Ok(name) = std::str::from_utf8(s) {
                        out.push(name.to_string());
                    }
                }
                break;
            }
        }
        i = p + 1;
    }
    out
}

fn name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'/' | b'@' | b'.' | b'+' | b'-')
}

fn uleb(buf: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut r: u64 = 0;
    let mut s = 0u32;
    loop {
        let &b = buf.get(i)?;
        i += 1;
        r |= u64::from(b & 0x7f) << s;
        if b & 0x80 == 0 {
            return Some((r, i));
        }
        s += 7;
        if s > 35 {
            return None;
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle, 0).is_some()
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8], chunk: usize) -> WasmScan {
        let mut s = WasmScan::new();
        for c in bytes.chunks(chunk.max(1)) {
            s.feed(c).unwrap();
        }
        s
    }

    fn uleb_enc(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn section(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(uleb_enc(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn component(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"\x00asm\x0d\x00\x01\x00".to_vec();
        for s in sections {
            out.extend_from_slice(s);
        }
        out
    }

    fn module(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"\x00asm\x01\x00\x00\x00".to_vec();
        for s in sections {
            out.extend_from_slice(s);
        }
        out
    }

    fn export_payload(name: &str) -> Vec<u8> {
        // filler, then uleb(len) + name, mirroring "length-prefixed names
        // sitting verbatim in the bytes"
        let mut p = vec![0x00, 0x01, 0x02];
        p.extend(uleb_enc(name.len() as u64));
        p.extend_from_slice(name.as_bytes());
        p.push(0x00);
        p
    }

    #[test]
    fn preamble_refusals() {
        let mut s = WasmScan::new();
        assert!(s.feed(b"MZobviously-not-wasm").is_err());
        let mut s = WasmScan::new();
        s.feed(b"\x00asm").unwrap();
        assert!(s.feed(b"\x0d\x00\x07\x00").is_err()); // layer 7
        let s = scan(b"\x00as", 1);
        assert!(s.accept_error().unwrap().contains("too small"));
    }

    #[test]
    fn core_module_needs_mem64() {
        // memory section: count=1, flags=0x00 (32-bit), min=1
        let m = module(&[section(5, &[0x01, 0x00, 0x01])]);
        let s = scan(&m, 3);
        assert!(s.accept_error().unwrap().contains("core wasm module"));
        // flags=0x04 (memory64), min=1
        let m = module(&[section(5, &[0x01, 0x04, 0x01])]);
        for chunk in [1, 2, 7, 1024] {
            let s = scan(&m, chunk);
            assert!(s.accept_error().is_none());
            assert!(s.verdict().mem64);
        }
    }

    #[test]
    fn component_worlds() {
        for (name, wasi) in [
            ("wasi:http/incoming-handler@0.2.0", Some("0.2")),
            ("wasi:http/handler@0.3.0", Some("0.3")),
            ("wasi:cli/run@0.2.0", Some("0.2")),
            ("wasi:cli/run@0.3.0-rc", Some("0.3")),
            ("wasi:random/random@0.2.0", None),
        ] {
            let c = component(&[section(11, &export_payload(name))]);
            for chunk in [1, 5, 4096] {
                let s = scan(&c, chunk);
                assert!(s.accept_error().is_none());
                let v = s.verdict();
                assert_eq!(v.wasi, wasi, "for {name}");
                if wasi.is_some() {
                    assert_eq!(v.world.as_deref(), Some(name));
                }
            }
        }
    }

    #[test]
    fn import_section_does_not_classify() {
        // The deciding name must be an EXPORT (section 11); the same name in
        // the import section (10) reports null, per the Python's export-only
        // scan order note.
        let c = component(&[section(10, &export_payload("wasi:http/incoming-handler@0.2.0"))]);
        let s = scan(&c, 9);
        assert_eq!(s.verdict().wasi, None);
    }

    #[test]
    fn markers_across_chunk_boundaries() {
        let mut body = component(&[section(11, &export_payload("wasi:cli/run@0.2.0"))]);
        body.extend_from_slice(b"xxxx[thread-new-indirect-v0]yyyy");
        body.extend_from_slice(b"zz[set-spawn-indirect]ww");
        for chunk in [1, 2, 3, 8, 64] {
            let s = scan(&body, chunk);
            let v = s.verdict();
            assert!(v.threads && v.set, "chunk={chunk}");
        }
    }

    #[test]
    fn big_skipped_sections_stay_bounded() {
        // A 10 MiB core-module data section (id 11 on a MODULE is 'data') is
        // walked through without storing; classification still works after.
        let mut body = component(&[section(3, &vec![0u8; 10 * 1024 * 1024])]);
        body.extend(section(11, &export_payload("wasi:http/incoming-handler@0.2.0")));
        let s = scan(&body, 1 << 16);
        assert!(s.export_payload.len() < KEEP_CAP);
        assert_eq!(s.verdict().wasi, Some("0.2"));
    }

    #[test]
    fn garbage_structure_classifies_null_never_refuses() {
        let mut body = b"\x00asm\x0d\x00\x01\x00".to_vec();
        body.extend_from_slice(&[0x0b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let s = scan(&body, 4);
        assert!(s.accept_error().is_none());
        assert_eq!(s.verdict().wasi, None);
    }

    #[test]
    fn real_component_classifies() {
        // The adapter's own build is a wasi:cli/run@0.2 command component;
        // classify it if the artifact exists (skip silently otherwise so
        // clean checkouts still pass).
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/wasm32-wasip2/release/s3-ipfs-adapter.wasm"
        );
        let Ok(bytes) = std::fs::read(p) else { return };
        let s = scan(&bytes, 1 << 16);
        assert!(s.accept_error().is_none());
        let v = s.verdict();
        assert_eq!(v.wasi, Some("0.2"));
        assert!(v.world.unwrap().starts_with("wasi:cli/run@0.2."));
    }
}
