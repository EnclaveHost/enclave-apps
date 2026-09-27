//! Trusted-side H.264 input transform offload. Only freshly masked blocks cross
//! the worker link. Nonlinear coding and the bitstream remain inside the guest.
use rand_core::{OsRng, RngCore};
use serde_json::json;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{Shutdown, TcpStream, ToSocketAddrs},
    time::Duration,
};

const MOD: i64 = 14_457_349;
const PRIMES: [i64; 3] = [251, 241, 239];
const VERIFY_PRIME: i64 = 2_147_483_647;
const ROUNDS: usize = 5;
pub const BATCH: usize = 2048;
const K: usize = 32;
const MAX_PIXELS: usize = 4096 * 2160;
const T: [[i64; 4]; 4] = [[1, 1, 1, 1], [2, 1, -1, -2], [1, -1, -1, 1], [1, -2, 2, -1]];
fn err(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn sample_many(modulus: u32, count: usize) -> io::Result<Vec<i64>> {
    let limit = u32::MAX - u32::MAX % modulus;
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        // Draw in bulk from the OS CSPRNG; reject the incomplete final interval
        // instead of introducing modular bias. No seed/counter is replayable.
        let mut bytes = vec![0u8; (count - out.len()).max(64) * 4];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| err("OS random source failed"))?;
        for word in bytes.chunks_exact(4) {
            let x = u32::from_le_bytes(word.try_into().unwrap());
            if x < limit {
                out.push((x % modulus) as i64);
            }
            if out.len() == count {
                break;
            }
        }
        bytes.fill(0);
    }
    Ok(out)
}
/// Exact 4x4 integer H.264 forward transform; no quantization or rounding.
pub fn transform(x: &[i64; 16]) -> [i64; 16] {
    let mut out = [0; 16];
    for v in 0..4 {
        for u in 0..4 {
            for y in 0..4 {
                for z in 0..4 {
                    out[v * 4 + u] += T[v][y] * x[y * 4 + z] * T[u][z];
                }
            }
        }
    }
    out
}
fn weights() -> [[i64; K]; K] {
    let mut w = [[0; K]; K];
    for i in 0..16 {
        let mut x = [0; 16];
        x[i] = 1;
        w[i][..16].copy_from_slice(&transform(&x));
    }
    w
}
fn fast_transform(x: &[i64; 16]) -> [i64; 16] {
    fn row(a: i64, b: i64, c: i64, d: i64) -> [i64; 4] {
        [
            a + b + c + d,
            2 * (a - d) + b - c,
            a + d - b - c,
            a - d - 2 * (b - c),
        ]
    }
    let mut tmp = [0; 16];
    let mut out = [0; 16];
    for y in 0..4 {
        tmp[y * 4..y * 4 + 4].copy_from_slice(&row(
            x[y * 4],
            x[y * 4 + 1],
            x[y * 4 + 2],
            x[y * 4 + 3],
        ));
    }
    for z in 0..4 {
        let r = row(tmp[z], tmp[4 + z], tmp[8 + z], tmp[12 + z]);
        for y in 0..4 {
            out[y * 4 + z] = r[y];
        }
    }
    out
}
/// A bounded worker transport: never retries a masked request. A failed client
/// is permanently poisoned. Reconnection requires fresh randomness and state.
pub struct Client {
    stream: TcpStream,
    dead: bool,
    pub exchanges: u64,
    pub masked_blocks: u64,
}
impl Client {
    pub fn connect(endpoint: &str) -> io::Result<Self> {
        let addr = endpoint
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| err("worker address is empty"))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_nodelay(true)?;
        let mut c = Self {
            stream,
            dead: false,
            exchanges: 0,
            masked_blocks: 0,
        };
        let hello = c.call(0, &1u32.to_le_bytes(), 65536)?;
        let _: serde_json::Value =
            serde_json::from_slice(&hello).map_err(|_| err("worker HELLO is not JSON"))?;
        for (bid, size, role) in [
            (1u64, 1088u64, "weights"),
            (2, (BATCH * K * 7) as u64, "activations"),
        ] {
            let mut p = size.to_le_bytes().to_vec();
            p.extend((role.len() as u32).to_le_bytes());
            p.extend(role.as_bytes());
            if c.call(1, &p, 8)? != bid.to_le_bytes() {
                return Err(err("unexpected worker allocation"));
            }
        }
        let w = weights();
        let mut q = Vec::with_capacity(1024);
        for row in &w {
            for v in row {
                q.push(*v as i8 as u8);
            }
        }
        c.set(1, 0, &q)?;
        // q8_0 scale = 1/256, so the existing worker's *256 encoding is exact.
        let scales: Vec<u8> = (0..K).flat_map(|_| 0x1c00u16.to_le_bytes()).collect();
        c.set(1, 1024, &scales)?;
        let yoff = BATCH * K * 3;
        let graph = json!({"nodes":[{"op":"FIELD_GEMM","id":"enclave-shield-video-h264-input-v1","K":K,"N":K,"max_m":BATCH,
            "wq":{"bid":1,"offset":0},"wd":{"bid":1,"offset":1024},"x":{"bid":2,"offset":0},"y":{"bid":2,"offset":yoff}}],
            "outputs":[{"bid":2,"offset":yoff,"nbytes":BATCH*K*4}]});
        c.call(10, &serde_json::to_vec(&graph).unwrap(), 65536)?;
        Ok(c)
    }
    fn set(&mut self, bid: u64, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut p = bid.to_le_bytes().to_vec();
        p.extend(offset.to_le_bytes());
        p.extend((data.len() as u64).to_le_bytes());
        p.extend(data);
        self.call(8, &p, 65536)?;
        Ok(())
    }
    fn call(&mut self, cmd: u8, p: &[u8], max: usize) -> io::Result<Vec<u8>> {
        if self.dead {
            return Err(err("worker connection is poisoned"));
        }
        let r = (|| {
            let mut request = Vec::with_capacity(9 + p.len());
            request.push(cmd);
            request.extend((p.len() as u64).to_le_bytes());
            request.extend(p);
            self.stream.write_all(&request)?;
            let mut h = [0; 9];
            self.stream.read_exact(&mut h)?;
            let n = u64::from_le_bytes(h[1..].try_into().unwrap());
            if h[0] != 0 || n > max as u64 {
                return Err(err("worker refused request or returned oversized response"));
            }
            let mut out = vec![0; n as usize];
            self.stream.read_exact(&mut out)?;
            Ok(out)
        })();
        if r.is_err() {
            self.poison();
        }
        r
    }
    fn poison(&mut self) {
        self.dead = true;
        let _ = self.stream.shutdown(Shutdown::Both);
    }
    /// Always a full fixed-size batch, including the last padded batch.
    fn batch(&mut self, blocks: &[[u8; 16]]) -> io::Result<Vec<[i16; 16]>> {
        if blocks.is_empty() || blocks.len() > BATCH || self.dead {
            return Err(err("invalid batch or poisoned client"));
        }
        let mut pad_values = sample_many(MOD as u32, BATCH * K)?;
        let mut pads = vec![[0i64; K]; BATCH];
        let mut planes = vec![0u8; 3 * BATCH * K];
        for m in 0..BATCH {
            for k in 0..K {
                pads[m][k] = pad_values[m * K + k];
                let x = if m < blocks.len() && k < 16 {
                    blocks[m][k] as i64
                } else {
                    0
                };
                let masked = (x + pads[m][k]) % MOD;
                for (p, q) in PRIMES.iter().enumerate() {
                    let r = masked % q;
                    planes[p * BATCH * K + m * K + k] =
                        (if r > q / 2 { r - q } else { r }) as i8 as u8;
                }
            }
        }
        pad_values.fill(0);
        let mut req = 1u32.to_le_bytes().to_vec();
        req.extend((BATCH as u32).to_le_bytes());
        req.extend(0u32.to_le_bytes());
        req.extend(planes);
        let reply = self.call(12, &req, BATCH * K * 4)?;
        if reply.len() != BATCH * K * 4 {
            self.poison();
            return Err(err("truncated transform reply"));
        }
        let mut clear = vec![[0i64; K]; BATCH];
        for m in 0..BATCH {
            let mut input = [0; 16];
            input.copy_from_slice(&pads[m][..16]);
            let unmask = fast_transform(&input);
            for k in 0..K {
                let i = (m * K + k) * 4;
                let raw = i32::from_le_bytes(reply[i..i + 4].try_into().unwrap()) as i64;
                if raw.abs() > MOD / 2 {
                    self.poison();
                    return Err(err("noncanonical field reply"));
                }
                let v = (raw - if k < 16 { unmask[k] } else { 0 }).rem_euclid(MOD);
                clear[m][k] = if v > MOD / 2 { v - MOD } else { v };
            }
        }
        pads.fill([0; K]);
        // Fresh CSPRNG challenges AFTER the full response arrives; all checks
        // complete before a coefficient is exposed to the codec.
        if let Err(e) = verify(blocks, &clear) {
            self.poison();
            return Err(e);
        }
        self.exchanges += 1;
        self.masked_blocks += BATCH as u64;
        Ok(clear[..blocks.len()]
            .iter()
            .map(|r| std::array::from_fn(|i| r[i] as i16))
            .collect())
    }
    pub fn frame(&mut self, y: &[u8], u: &[u8], v: &[u8], w: usize, h: usize) -> io::Result<Frame> {
        let result = self.frame_inner(y, u, v, w, h);
        if result.is_err() {
            self.poison();
        }
        result
    }
    fn frame_inner(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        w: usize,
        h: usize,
    ) -> io::Result<Frame> {
        if w == 0
            || h == 0
            || w % 2 != 0
            || h % 2 != 0
            || w.checked_mul(h).filter(|n| *n <= MAX_PIXELS).is_none()
        {
            return Err(err("invalid dimensions"));
        }
        if y.len() != w * h || u.len() != w * h / 4 || v.len() != w * h / 4 {
            return Err(err("invalid plane lengths"));
        }
        let mut blocks = Vec::new();
        for (plane, pw, ph) in [(y, w, h), (u, w / 2, h / 2), (v, w / 2, h / 2)] {
            for by in (0..ph).step_by(4) {
                for bx in (0..pw).step_by(4) {
                    blocks.push(std::array::from_fn(|i| {
                        plane[(by + i / 4).min(ph - 1) * pw + (bx + i % 4).min(pw - 1)]
                    }));
                }
            }
        }
        let mut map = HashMap::with_capacity(blocks.len());
        for part in blocks.chunks(BATCH) {
            let coeff = self.batch(part)?;
            for (block, t) in part.iter().zip(coeff) {
                map.insert(*block, t);
            }
        }
        Ok(Frame {
            map,
            hits: 0,
            misses: 0,
        })
    }
}
fn verify(blocks: &[[u8; 16]], ys: &[[i64; K]]) -> io::Result<()> {
    if ys.len() != BATCH {
        return Err(err("invalid verification shape"));
    }
    for y in ys {
        if y[..16].iter().any(|v| v.abs() > 9180) || y[16..].iter().any(|v| *v != 0) {
            return Err(err("transform outside exact bounds"));
        }
    }
    let w = weights();
    for _ in 0..ROUNDS {
        let s = sample_many(VERIFY_PRIME as u32, 16)?;
        let ws: [i64; 16] = std::array::from_fn(|i| (0..16).map(|j| w[i][j] * s[j]).sum::<i64>());
        for (i, y) in ys.iter().enumerate() {
            let lhs: i64 = (0..16).map(|j| y[j] * s[j]).sum();
            let rhs: i64 = if i < blocks.len() {
                (0..16).map(|j| blocks[i][j] as i64 * ws[j]).sum()
            } else {
                0
            };
            if (lhs - rhs).rem_euclid(VERIFY_PRIME) != 0 {
                return Err(err("masked video integrity check failed"));
            }
        }
    }
    Ok(())
}
/// Plaintext cache lives exclusively with the codec, never in the worker.
/// Lookup/misses do not issue network traffic. Denoised/edge variants can use
/// the existing CPU transform without exposing content-dependent GPU requests.
pub struct Frame {
    map: HashMap<[u8; 16], [i16; 16]>,
    pub hits: u64,
    pub misses: u64,
}
impl Frame {
    pub fn residual(&mut self, input: [u8; 16], pred: [u8; 16]) -> Option<[i16; 16]> {
        let Some(t) = self.map.get(&input) else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        let p = fast_transform(&pred.map(|v| v as i64));
        Some(std::array::from_fn(|i| (t[i] as i64 - p[i]) as i16))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transform_basis_and_extremes() {
        for i in 0..16 {
            let mut x = [0; 16];
            x[i] = 1;
            assert_eq!(transform(&x), fast_transform(&x));
        }
        for value in [0, 255, MOD - 1] {
            let x = [value; 16];
            assert_eq!(transform(&x), fast_transform(&x));
        }
    }
    #[test]
    fn scoped_frame_rejects_nesting_and_cleans_up() {
        fn frame() -> Frame {
            Frame {
                map: HashMap::new(),
                hits: 0,
                misses: 0,
            }
        }
        with_frame(frame(), || assert!(with_frame(frame(), || ()).is_err())).unwrap();
        let result = std::panic::catch_unwind(|| with_frame(frame(), || panic!("test unwind")));
        assert!(result.is_err());
        with_frame(frame(), || ()).unwrap();
    }
    #[test]
    fn verification_rejects_corruption_and_padding() {
        let b = vec![[123u8; 16]; 2];
        let mut y = vec![[0i64; K]; BATCH];
        for i in 0..2 {
            y[i][..16].copy_from_slice(&transform(&[123; 16]));
        }
        verify(&b, &y).unwrap();
        y[0][1] += 1;
        assert!(verify(&b, &y).is_err());
        y[0][1] -= 1;
        y[2][0] = 1;
        assert!(verify(&b, &y).is_err());
    }
}

thread_local! { static ACTIVE:std::cell::RefCell<Option<Frame>>=const { std::cell::RefCell::new(None) }; }
/// Scope one verified frame to the synchronous codec call on this thread.
/// Panic unwinding clears it; nested use is rejected before changing state.
pub fn with_frame<R>(frame: Frame, f: impl FnOnce() -> R) -> io::Result<(R, u64, u64)> {
    ACTIVE.with(|a| {
        let mut a = a.borrow_mut();
        if a.is_some() {
            return Err(err("nested video frame"));
        }
        *a = Some(frame);
        Ok(())
    })?;
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            ACTIVE.with(|a| {
                a.borrow_mut().take();
            });
        }
    }
    let guard = Guard;
    let r = f();
    let stats = ACTIVE.with(|a| {
        let b = a.borrow();
        let f = b.as_ref().unwrap();
        (f.hits, f.misses)
    });
    drop(guard);
    Ok((r, stats.0, stats.1))
}
/// Called only by minih264 with four readable input/predictor rows and 16
/// writable i16 outputs. No unwind, I/O, secret logging, or allocation here.
///
/// # Safety
/// `inp` must cover four rows of `stride` bytes; `pred` four rows of 16 bytes;
/// `out` must have room for 16 aligned i16 values. Buffers must not alias.
#[no_mangle]
pub unsafe extern "C" fn rbx_shield_transform(
    inp: *const u8,
    pred: *const u8,
    stride: u32,
    out: *mut i16,
    transpose: i32,
) -> i32 {
    if inp.is_null() || pred.is_null() || out.is_null() {
        return 0;
    }
    ACTIVE.with(|a| {
        let Ok(mut slot) = a.try_borrow_mut() else {
            return 0;
        };
        let Some(f) = slot.as_mut() else {
            return 0;
        };
        let input = std::array::from_fn(|i| *inp.add(i / 4 * stride as usize + i % 4));
        let prediction = std::array::from_fn(|i| *pred.add(i / 4 * 16 + i % 4));
        let Some(t) = f.residual(input, prediction) else {
            return 0;
        };
        for i in 0..16 {
            *out.add(i) = t[if transpose != 0 {
                (i % 4) * 4 + i / 4
            } else {
                i
            }];
        }
        1
    })
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    fn fake(
        mode: &'static str,
    ) -> (
        String,
        Arc<Mutex<Vec<Vec<u8>>>>,
        std::thread::JoinHandle<()>,
    ) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(vec![]));
        let saved = seen.clone();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut bid = 0u64;
            let mut previous: Option<Vec<u8>> = None;
            loop {
                let mut h = [0; 9];
                if s.read_exact(&mut h).is_err() {
                    break;
                }
                let n = u64::from_le_bytes(h[1..].try_into().unwrap()) as usize;
                assert!(n <= 12 + 3 * BATCH * K);
                let mut p = vec![0; n];
                s.read_exact(&mut p).unwrap();
                let mut reply = match h[0] {
                    0 => b"{\"version\":[1,4,0]}".to_vec(),
                    1 => {
                        bid += 1;
                        bid.to_le_bytes().to_vec()
                    }
                    8 => vec![],
                    10 => b"{}".to_vec(),
                    12 => {
                        assert_eq!(p.len(), 12 + 3 * BATCH * K);
                        saved.lock().unwrap().push(p.clone());
                        if mode == "oversized" {
                            s.write_all(&[0]).unwrap();
                            s.write_all(&u64::MAX.to_le_bytes()).unwrap();
                            break;
                        }
                        if mode == "disconnect" {
                            break;
                        }
                        let w = weights();
                        let mut result = Vec::new();
                        for m in 0..BATCH {
                            for out in 0..K {
                                let mut rs = [0i64; 3];
                                for k in 0..K {
                                    for q in 0..3 {
                                        rs[q] += (p[12 + q * BATCH * K + m * K + k] as i8 as i64)
                                            * w[k][out];
                                    }
                                }
                                let mut v = rs[0].rem_euclid(251);
                                v += 251 * ((rs[1] - v) * 217).rem_euclid(241);
                                // inverse of 251*241 mod239 computed independently, not a magic wire value.
                                let inv = (1..239).find(|i| (251 * 241 * i) % 239 == 1).unwrap();
                                v += 251 * 241 * ((rs[2] - v) * inv).rem_euclid(239);
                                if v > MOD / 2 {
                                    v -= MOD;
                                }
                                result.extend((v as i32).to_le_bytes());
                            }
                        }
                        if mode == "replay" {
                            if let Some(old) = &previous {
                                result = old.clone();
                            } else {
                                previous = Some(result.clone());
                            }
                        }
                        result
                    }
                    _ => panic!("unexpected command"),
                };
                if h[0] == 12 && mode == "corrupt" {
                    let x = i32::from_le_bytes(reply[..4].try_into().unwrap());
                    reply[..4].copy_from_slice(
                        &(if x == MOD as i32 / 2 { x - 1 } else { x + 1 }).to_le_bytes(),
                    );
                }
                if h[0] == 12 && mode == "noncanonical" {
                    reply[..4].copy_from_slice(&i32::MAX.to_le_bytes());
                }
                if h[0] == 12 && mode == "short" {
                    reply.truncate(4);
                }
                if s.write_all(&[0])
                    .and_then(|_| s.write_all(&(reply.len() as u64).to_le_bytes()))
                    .and_then(|_| s.write_all(&reply))
                    .is_err()
                {
                    break;
                }
            }
        });
        (addr, seen, t)
    }
    #[test]
    fn fixed_batches_and_fresh_masks() {
        let (addr, seen, t) = fake("ok");
        let mut c = Client::connect(&addr).unwrap();
        let b = vec![[42; 16]; 3];
        let x = c.batch(&b).unwrap();
        assert_eq!(x[0], transform(&[42; 16]).map(|v| v as i16));
        assert_eq!(c.batch(&b).unwrap(), x);
        drop(c);
        t.join().unwrap();
        let p = seen.lock().unwrap();
        assert_eq!(p[0].len(), p[1].len());
        assert_ne!(p[0][12..], p[1][12..]);
    }
    #[test]
    fn invalid_frame_emits_no_blocks_and_poisons() {
        for (w, h, y) in [
            (0, 2, vec![]),
            (3, 2, vec![0; 6]),
            (4096, 2162, vec![]),
            (2, 2, vec![0; 3]),
        ] {
            let (addr, seen, t) = fake("ok");
            let mut c = Client::connect(&addr).unwrap();
            assert!(c.frame(&y, &[0], &[0], w, h).is_err());
            assert!(c.frame(&[0; 4], &[0], &[0], 2, 2).is_err());
            assert!(seen.lock().unwrap().is_empty());
            drop(c);
            t.join().unwrap();
        }
    }
    #[test]
    fn malicious_workers_poison_connection() {
        for mode in [
            "corrupt",
            "oversized",
            "disconnect",
            "replay",
            "noncanonical",
            "short",
        ] {
            let (addr, seen, t) = fake(mode);
            let mut c = Client::connect(&addr).unwrap();
            let b = vec![[255; 16]; 2];
            if mode == "replay" {
                c.batch(&b).unwrap();
            }
            assert!(c.batch(&b).is_err(), "{mode}");
            let n = seen.lock().unwrap().len();
            assert!(c.batch(&b).is_err());
            assert_eq!(seen.lock().unwrap().len(), n);
            drop(c);
            t.join().unwrap();
        }
    }
}
