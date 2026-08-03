// Pure-Rust SNAC 24 kHz decoder (hubertsiuzdak/snac_24khz).
//
// Decodes the three SNAC codebook streams (12 / 23 / 47 Hz) into 24 kHz mono
// f32 audio entirely on the CPU. No dependencies. Weights come from a
// "SNACDEC1" container produced by tools/export_snac.py: weight-norm fused,
// f16 payload, self-describing tensor table.
//
// Architecture (decoder_dim 1024, rates [8,8,4,2], depthwise, noise, no attn):
//   from_codes: per codebook, embed(4096x8) -> 1x1 proj to 768 -> repeat by
//               vq_stride [4,2,1], summed into z(768, T)
//   decoder:    dw-conv k7 + pw 768->1024, then 4 blocks
//               [snake, tconv k=2s stride s, noise, 3 res units d=1,3,9],
//               1024->512->256->128->64, then snake + conv k7 ->1 + tanh.
// One 7-token frame = 4 latent steps = 2048 samples (85.3 ms).

pub const SAMPLE_RATE: u32 = 24_000;
#[allow(dead_code)] // the streaming contract, asserted in tests
pub const FRAME_SAMPLES: usize = 2048;
/// Latent steps per 7-token frame (decoder upsamples each by 512 samples).
pub const LATENTS_PER_FRAME: usize = 4;
const CODEBOOK: usize = 4096;
const CB_DIM: usize = 8;
const LATENT: usize = 768;
const STRIDES: [usize; 4] = [8, 8, 4, 2];
const DIMS: [usize; 5] = [1024, 512, 256, 128, 64];
const VQ_STRIDES: [usize; 3] = [4, 2, 1];

/// Codebook index streams for N frames: l1 = N, l2 = 2N, l3 = 4N entries.
pub struct Codes {
    pub l1: Vec<u16>,
    pub l2: Vec<u16>,
    pub l3: Vec<u16>,
}

impl Codes {
    pub fn frames(&self) -> usize {
        self.l1.len()
    }
    pub(crate) fn ok(&self) -> bool {
        !self.l1.is_empty()
            && self.l2.len() == 2 * self.l1.len()
            && self.l3.len() == 4 * self.l1.len()
            && self.l1.iter().all(|&c| (c as usize) < CODEBOOK)
            && self.l2.iter().all(|&c| (c as usize) < CODEBOOK)
            && self.l3.iter().all(|&c| (c as usize) < CODEBOOK)
    }
}

struct ResUnit {
    snake1: Vec<f32>,
    dw_w: Vec<f32>,
    dw_b: Vec<f32>,
    snake2: Vec<f32>,
    pw_w: Vec<f32>,
    pw_b: Vec<f32>,
    dilation: usize,
}

struct Block {
    c_in: usize,
    c_out: usize,
    stride: usize,
    snake_in: Vec<f32>,
    tconv_w: Vec<f32>, // (c_in, c_out, 2*stride)
    tconv_b: Vec<f32>,
    noise_w: Vec<f32>, // (c_out, c_out) 1x1, no bias
    res: [ResUnit; 3],
}

pub struct Decoder {
    codebooks: [Vec<f32>; 3],  // (4096, 8)
    out_proj_w: [Vec<f32>; 3], // (768, 8)
    out_proj_b: [Vec<f32>; 3], // (768,)
    in_dw_w: Vec<f32>,         // (768, 7)
    in_dw_b: Vec<f32>,
    in_pw_w: Vec<f32>, // (1024, 768)
    in_pw_b: Vec<f32>,
    blocks: [Block; 4],
    out_snake: Vec<f32>, // (64,)
    out_w: Vec<f32>,     // (64, 7)
    out_b: f32,
}

// ---------------------------------------------------------------- container

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if frac == 0 {
            sign << 31
        } else {
            // subnormal: normalize
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            (sign << 31) | ((e as u32) << 23) | ((f & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        (sign << 31) | (0xff << 23) | (frac << 13)
    } else {
        (sign << 31) | ((exp + 127 - 15) << 23) | (frac << 13)
    };
    f32::from_bits(bits)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err("snac weights: truncated".into());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

struct RawTensor {
    shape: Vec<usize>,
    data: Vec<f32>,
}

fn parse_container(bytes: &[u8]) -> Result<Vec<(String, RawTensor)>, String> {
    let mut r = Reader { buf: bytes, pos: 0 };
    if r.take(8)? != b"SNACDEC1" {
        return Err("snac weights: bad magic".into());
    }
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let name_len = r.u16()? as usize;
        let name = String::from_utf8(r.take(name_len)?.to_vec())
            .map_err(|_| "snac weights: bad name")?;
        let dtype = r.take(1)?[0];
        let ndim = r.take(1)?[0] as usize;
        let mut shape = Vec::with_capacity(ndim);
        for _ in 0..ndim {
            shape.push(r.u32()? as usize);
        }
        let nbytes = r.u64()? as usize;
        let payload = r.take(nbytes)?;
        let numel: usize = shape.iter().product();
        let data: Vec<f32> = match dtype {
            0 => {
                if nbytes != numel * 2 {
                    return Err(format!("snac weights: {name} size mismatch"));
                }
                payload
                    .chunks_exact(2)
                    .map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]])))
                    .collect()
            }
            1 => {
                if nbytes != numel * 4 {
                    return Err(format!("snac weights: {name} size mismatch"));
                }
                payload
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                    .collect()
            }
            _ => return Err(format!("snac weights: {name} unknown dtype")),
        };
        out.push((name, RawTensor { shape, data }));
    }
    Ok(out)
}

// ------------------------------------------------------------------ loading

struct TensorMap(std::collections::HashMap<String, RawTensor>);

impl TensorMap {
    fn get(&mut self, name: &str, shape: &[usize]) -> Result<Vec<f32>, String> {
        let t = self
            .0
            .remove(name)
            .ok_or_else(|| format!("snac weights: missing tensor {name}"))?;
        // squeeze singleton dims for comparison (e.g. (768,8,1) vs (768,8))
        let got: Vec<usize> = t.shape.iter().copied().filter(|&d| d != 1).collect();
        let want: Vec<usize> = shape.iter().copied().filter(|&d| d != 1).collect();
        if got != want {
            return Err(format!(
                "snac weights: {name} shape {:?} != expected {:?}",
                t.shape, shape
            ));
        }
        Ok(t.data)
    }
}

impl Decoder {
    pub fn from_bytes(bytes: &[u8]) -> Result<Decoder, String> {
        let mut m = TensorMap(parse_container(bytes)?.into_iter().collect());

        let mut codebooks: [Vec<f32>; 3] = Default::default();
        let mut out_proj_w: [Vec<f32>; 3] = Default::default();
        let mut out_proj_b: [Vec<f32>; 3] = Default::default();
        for q in 0..3 {
            codebooks[q] = m.get(&format!("q{q}.codebook"), &[CODEBOOK, CB_DIM])?;
            out_proj_w[q] = m.get(&format!("q{q}.out_proj.w"), &[LATENT, CB_DIM])?;
            out_proj_b[q] = m.get(&format!("q{q}.out_proj.b"), &[LATENT])?;
        }

        let in_dw_w = m.get("dec.in_dw.w", &[LATENT, 7])?;
        let in_dw_b = m.get("dec.in_dw.b", &[LATENT])?;
        let in_pw_w = m.get("dec.in_pw.w", &[DIMS[0], LATENT])?;
        let in_pw_b = m.get("dec.in_pw.b", &[DIMS[0]])?;

        let mut blocks = Vec::with_capacity(4);
        for bi in 0..4 {
            let (c_in, c_out, stride) = (DIMS[bi], DIMS[bi + 1], STRIDES[bi]);
            let b = format!("blk{bi}");
            let mut res = Vec::with_capacity(3);
            for (ru, dil) in [1usize, 3, 9].iter().enumerate() {
                let r = format!("{b}.res{ru}");
                res.push(ResUnit {
                    snake1: m.get(&format!("{r}.snake1.alpha"), &[c_out])?,
                    dw_w: m.get(&format!("{r}.dw.w"), &[c_out, 7])?,
                    dw_b: m.get(&format!("{r}.dw.b"), &[c_out])?,
                    snake2: m.get(&format!("{r}.snake2.alpha"), &[c_out])?,
                    pw_w: m.get(&format!("{r}.pw.w"), &[c_out, c_out])?,
                    pw_b: m.get(&format!("{r}.pw.b"), &[c_out])?,
                    dilation: *dil,
                });
            }
            blocks.push(Block {
                c_in,
                c_out,
                stride,
                snake_in: m.get(&format!("{b}.snake_in.alpha"), &[c_in])?,
                tconv_w: m.get(&format!("{b}.tconv.w"), &[c_in, c_out, 2 * stride])?,
                tconv_b: m.get(&format!("{b}.tconv.b"), &[c_out])?,
                noise_w: m.get(&format!("{b}.noise.w"), &[c_out, c_out])?,
                res: match res.try_into() {
                    Ok(a) => a,
                    Err(_) => unreachable!(),
                },
            });
        }

        let out_snake = m.get("dec.out_snake.alpha", &[DIMS[4]])?;
        let out_w = m.get("dec.out.w", &[DIMS[4], 7])?;
        let out_b = m.get("dec.out.b", &[1])?[0];

        Ok(Decoder {
            codebooks,
            out_proj_w,
            out_proj_b,
            in_dw_w,
            in_dw_b,
            in_pw_w,
            in_pw_b,
            blocks: match blocks.try_into() {
                Ok(a) => a,
                Err(_) => unreachable!(),
            },
            out_snake,
            out_w,
            out_b,
        })
    }
}

// --------------------------------------------------------------------- ops

/// Branch-free cos on r in [-pi, pi] (Taylor through r^14, |err| < 5e-6),
/// with round-based range reduction. Autovectorizes; ~10x faster than libm.
#[inline(always)]
fn fast_cos(y: f32) -> f32 {
    const INV_2PI: f32 = 1.0 / (2.0 * std::f32::consts::PI);
    const TWO_PI: f32 = 2.0 * std::f32::consts::PI;
    let r = y - (y * INV_2PI).round() * TWO_PI;
    let r2 = r * r;
    // 1 - r2/2! + r2^2/4! - ... + r2^7/14!  (Horner)
    let mut p = -1.0 / 87_178_291_200.0f32; // -1/14!
    p = p * r2 + 1.0 / 479_001_600.0; // 1/12!
    p = p * r2 - 1.0 / 3_628_800.0; // -1/10!
    p = p * r2 + 1.0 / 40_320.0; // 1/8!
    p = p * r2 - 1.0 / 720.0; // -1/6!
    p = p * r2 + 1.0 / 24.0; // 1/4!
    p = p * r2 - 0.5; // -1/2!
    p * r2 + 1.0
}

/// x += (1/(a+eps)) * sin^2(a*x) == x + (1 - cos(2ax)) / (2*(a+eps)), per row.
fn snake_inplace(x: &mut [f32], c: usize, t: usize, alpha: &[f32]) {
    debug_assert_eq!(x.len(), c * t);
    for ch in 0..c {
        let a = alpha[ch];
        let half_inv = 0.5 / (a + 1e-9);
        let two_a = 2.0 * a;
        for v in &mut x[ch * t..(ch + 1) * t] {
            *v += half_inv * (1.0 - fast_cos(two_a * *v));
        }
    }
}

/// Depthwise conv k=7, pad 3*dilation (length-preserving).
fn conv_dw(x: &[f32], c: usize, t: usize, w: &[f32], b: &[f32], dil: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; c * t];
    for ch in 0..c {
        let xr = &x[ch * t..(ch + 1) * t];
        let or = &mut out[ch * t..(ch + 1) * t];
        let wr = &w[ch * 7..ch * 7 + 7];
        let bias = b[ch];
        or.fill(bias);
        for (k, &wv) in wr.iter().enumerate() {
            if wv == 0.0 {
                continue;
            }
            let off = k as isize * dil as isize - 3 * dil as isize;
            let (dst0, src0) = if off < 0 {
                ((-off) as usize, 0usize)
            } else {
                (0usize, off as usize)
            };
            let n = t - dst0.max(src0);
            let (dst, src) = (&mut or[dst0..dst0 + n], &xr[src0..src0 + n]);
            for i in 0..n {
                dst[i] += wv * src[i];
            }
        }
    }
    out
}

// Tiled matmul microkernel: the workhorse behind pointwise and transposed
// convs. Computes out(c_out, t) = W(c_out, c_in) . x(c_in, t) + bias in
// t-tiles of TB columns and 4 output rows at a time, so each x load feeds
// 4 FMAs and accumulators stay in registers. Autovectorizes cleanly.
const TB: usize = 128;

fn conv_pw(x: &[f32], c_in: usize, t: usize, w: &[f32], b: Option<&[f32]>, c_out: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; c_out * t];
    let mut acc = [[0.0f32; TB]; 4];
    let mut t0 = 0;
    while t0 < t {
        let tb = (t - t0).min(TB);
        let mut co = 0;
        while co < c_out {
            let nr = (c_out - co).min(4);
            for j in 0..nr {
                acc[j][..tb].fill(b.map_or(0.0, |bias| bias[co + j]));
            }
            for ci in 0..c_in {
                let xr = &x[ci * t + t0..ci * t + t0 + tb];
                for j in 0..nr {
                    let wv = w[(co + j) * c_in + ci];
                    let a = &mut acc[j];
                    for i in 0..tb {
                        a[i] += wv * xr[i];
                    }
                }
            }
            for j in 0..nr {
                out[(co + j) * t + t0..(co + j) * t + t0 + tb].copy_from_slice(&acc[j][..tb]);
            }
            co += nr;
        }
        t0 += TB;
    }
    out
}

/// ConvTranspose1d k=2*stride, stride s, pad s/2 (even strides): t -> t*s.
///
/// Phase decomposition: output position o = q*s + r depends on exactly two
/// input steps, ti = q+d and q+d-1, where d = (r >= s/2) as usize, through
/// weights w[ci][co][rr] and w[ci][co][rr+s] with rr = (r + s/2) % s. Each
/// phase r is therefore a 2-tap dense matmul with contiguous output.
fn tconv(x: &[f32], c_in: usize, t: usize, w: &[f32], b: &[f32], c_out: usize, s: usize) -> Vec<f32> {
    let k = 2 * s;
    let pad = s / 2;
    let t_out = t * s;
    let mut out = vec![0.0f32; c_out * t_out];
    // zero-pad one column each side so both taps are always in-bounds:
    // xp[ci][i] = x[ci][i-1], xp[ci][0] = xp[ci][t+1] = 0
    let tp = t + 2;
    let mut xp = vec![0.0f32; c_in * tp];
    for ci in 0..c_in {
        xp[ci * tp + 1..ci * tp + 1 + t].copy_from_slice(&x[ci * t..(ci + 1) * t]);
    }
    let mut wa = vec![0.0f32; c_out * c_in];
    let mut wb = vec![0.0f32; c_out * c_in];
    let mut phase = vec![0.0f32; c_out * t];
    let mut acc = [[0.0f32; TB]; 4];
    for r in 0..s {
        let rr = (r + pad) % s;
        let d = (r + pad) / s; // 0 or 1
        for co in 0..c_out {
            for ci in 0..c_in {
                wa[co * c_in + ci] = w[(ci * c_out + co) * k + rr]; // tap at ti = q+d
                wb[co * c_in + ci] = w[(ci * c_out + co) * k + rr + s]; // tap at ti = q+d-1
            }
        }
        let mut t0 = 0;
        while t0 < t {
            let tb = (t - t0).min(TB);
            let mut co = 0;
            while co < c_out {
                let nr = (c_out - co).min(4);
                for j in 0..nr {
                    acc[j][..tb].fill(b[co + j]);
                }
                for ci in 0..c_in {
                    // A tap x[q+d] -> xp[q+d+1]; B tap x[q+d-1] -> xp[q+d]
                    let base = ci * tp + t0 + d;
                    let xb = &xp[base..base + tb + 1];
                    for j in 0..nr {
                        let (wva, wvb) = (wa[(co + j) * c_in + ci], wb[(co + j) * c_in + ci]);
                        let a = &mut acc[j];
                        for i in 0..tb {
                            a[i] += wva * xb[i + 1] + wvb * xb[i];
                        }
                    }
                }
                for j in 0..nr {
                    phase[(co + j) * t + t0..(co + j) * t + t0 + tb].copy_from_slice(&acc[j][..tb]);
                }
                co += nr;
            }
            t0 += TB;
        }
        // scatter phase rows into strided output positions
        for co in 0..c_out {
            let pr = &phase[co * t..(co + 1) * t];
            let or = &mut out[co * t_out..(co + 1) * t_out];
            for q in 0..t {
                or[q * s + r] = pr[q];
            }
        }
    }
    out
}

/// Final conv: c_in channels -> 1, k=7, pad 3.
fn conv_out(x: &[f32], c_in: usize, t: usize, w: &[f32], b: f32) -> Vec<f32> {
    let mut out = vec![b; t];
    for ci in 0..c_in {
        let xr = &x[ci * t..(ci + 1) * t];
        let wr = &w[ci * 7..ci * 7 + 7];
        for (k, &wv) in wr.iter().enumerate() {
            if wv == 0.0 {
                continue;
            }
            let off = k as isize - 3;
            let (dst0, src0) = if off < 0 {
                ((-off) as usize, 0usize)
            } else {
                (0usize, off as usize)
            };
            let n = t - dst0.max(src0);
            for i in 0..n {
                out[dst0 + i] += wv * xr[src0 + i];
            }
        }
    }
    out
}

// A small PCG32 for the decoder's noise blocks: deterministic per seed.
struct Pcg {
    state: u64,
    spare: Option<f32>,
}

impl Pcg {
    fn new(seed: u64) -> Pcg {
        let mut p = Pcg { state: seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407), spare: None };
        p.next_u32();
        p
    }
    fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }
    fn uniform(&mut self) -> f32 {
        // (0, 1]: avoid ln(0)
        ((self.next_u32() >> 8) as f32 + 1.0) / 16_777_217.0
    }
    /// Standard normal via Box-Muller.
    fn gauss(&mut self) -> f32 {
        if let Some(v) = self.spare.take() {
            return v;
        }
        let (u1, u2) = (self.uniform(), self.uniform());
        let r = (-2.0 * u1.ln()).sqrt();
        let (s, c) = (2.0 * std::f32::consts::PI * u2).sin_cos();
        self.spare = Some(r * s);
        r * c
    }
}

// ------------------------------------------------------------------ decode

impl Decoder {
    /// z(768, T) from the three code streams; `lo..hi` selects a latent range.
    fn from_codes(&self, codes: &Codes, lo: usize, hi: usize) -> Vec<f32> {
        let t = hi - lo;
        let mut z = vec![0.0f32; LATENT * t];
        for q in 0..3 {
            let st = VQ_STRIDES[q];
            let cb = &self.codebooks[q];
            let w = &self.out_proj_w[q];
            let b = &self.out_proj_b[q];
            let stream: &[u16] = match q {
                0 => &codes.l1,
                1 => &codes.l2,
                _ => &codes.l3,
            };
            // latent positions lo..hi map to code indices lo/st..ceil(hi/st)
            let i0 = lo / st;
            let i1 = (hi + st - 1) / st;
            for i in i0..i1 {
                let emb = &cb[(stream[i] as usize) * CB_DIM..(stream[i] as usize) * CB_DIM + CB_DIM];
                // latent span covered by this code, clipped to [lo, hi)
                let p0 = (i * st).max(lo) - lo;
                let p1 = ((i + 1) * st).min(hi) - lo;
                for row in 0..LATENT {
                    let wr = &w[row * CB_DIM..row * CB_DIM + CB_DIM];
                    let mut v = b[row];
                    for j in 0..CB_DIM {
                        v += wr[j] * emb[j];
                    }
                    let zr = &mut z[row * t + p0..row * t + p1];
                    for e in zr {
                        *e += v;
                    }
                }
            }
        }
        z
    }

    /// Decode latent range [lo, hi) to (hi-lo)*512 samples.
    fn decode_range(&self, codes: &Codes, lo: usize, hi: usize, rng: &mut Option<Pcg>) -> Vec<f32> {
        let mut t = hi - lo;
        let z = self.from_codes(codes, lo, hi);
        let x = conv_dw(&z, LATENT, t, &self.in_dw_w, &self.in_dw_b, 1);
        let mut x = conv_pw(&x, LATENT, t, &self.in_pw_w, Some(&self.in_pw_b), DIMS[0]);
        for blk in &self.blocks {
            snake_inplace(&mut x, blk.c_in, t, &blk.snake_in);
            let mut y = tconv(&x, blk.c_in, t, &blk.tconv_w, &blk.tconv_b, blk.c_out, blk.stride);
            t *= blk.stride;
            if let Some(r) = rng {
                let lin = conv_pw(&y, blk.c_out, t, &blk.noise_w, None, blk.c_out);
                let noise: Vec<f32> = (0..t).map(|_| r.gauss()).collect();
                for c in 0..blk.c_out {
                    let yr = &mut y[c * t..(c + 1) * t];
                    let lr = &lin[c * t..(c + 1) * t];
                    for i in 0..t {
                        yr[i] += noise[i] * lr[i];
                    }
                }
            }
            for ru in &blk.res {
                let mut h = y.clone();
                snake_inplace(&mut h, blk.c_out, t, &ru.snake1);
                let mut h = conv_dw(&h, blk.c_out, t, &ru.dw_w, &ru.dw_b, ru.dilation);
                snake_inplace(&mut h, blk.c_out, t, &ru.snake2);
                let h = conv_pw(&h, blk.c_out, t, &ru.pw_w, Some(&ru.pw_b), blk.c_out);
                for i in 0..y.len() {
                    y[i] += h[i];
                }
            }
            x = y;
        }
        snake_inplace(&mut x, DIMS[4], t, &self.out_snake);
        let mut audio = conv_out(&x, DIMS[4], t, &self.out_w, self.out_b);
        for v in &mut audio {
            *v = v.tanh();
        }
        audio
    }

    /// Decode everything in one pass. Memory scales with length; prefer
    /// `decode` for anything longer than a few seconds. (The app streams via
    /// `decode_frames`; these two are the whole-take API the golden-vector
    /// harness in the model-volume workflow validates against.)
    #[allow(dead_code)]
    pub fn decode_full(&self, codes: &Codes, noise_seed: Option<u64>) -> Result<Vec<f32>, String> {
        if !codes.ok() {
            return Err("snac: malformed code streams".into());
        }
        let mut rng = noise_seed.map(Pcg::new);
        Ok(self.decode_range(codes, 0, codes.l3.len(), &mut rng))
    }

    /// Chunked decode: bounded memory, output identical to `decode_full`
    /// up to the (inaudible) noise-block randomness. Chunk/halo are in
    /// latent steps; halo 24 comfortably covers the decoder's receptive
    /// field (~10 latents).
    #[allow(dead_code)]
    pub fn decode(&self, codes: &Codes, noise_seed: Option<u64>) -> Result<Vec<f32>, String> {
        const CHUNK: usize = 192; // 4.1 s of audio per chunk
        const HALO: usize = 24;
        if !codes.ok() {
            return Err("snac: malformed code streams".into());
        }
        let t_total = codes.l3.len();
        if t_total <= CHUNK + 2 * HALO {
            return self.decode_full(codes, noise_seed);
        }
        let mut out = Vec::with_capacity(t_total * 512);
        let mut start = 0usize;
        let mut chunk_idx = 0u64;
        while start < t_total {
            let end = (start + CHUNK).min(t_total);
            let lo = start.saturating_sub(HALO) & !3; // keep l1 (stride 4) aligned
            let hi = (end + HALO).min(t_total);
            let mut rng = noise_seed.map(|s| Pcg::new(s ^ chunk_idx.wrapping_mul(0x9e3779b97f4a7c15)));
            let audio = self.decode_range(codes, lo, hi, &mut rng);
            let head = (start - lo) * 512;
            let tail = (hi - end) * 512;
            out.extend_from_slice(&audio[head..audio.len() - tail]);
            start = end;
            chunk_idx += 1;
        }
        Ok(out)
    }

    /// Streaming variant: decode only frames [frame_lo, frame_hi) with proper
    /// halo context from the full code streams (for sentence-chunk streaming).
    pub fn decode_frames(
        &self,
        codes: &Codes,
        frame_lo: usize,
        frame_hi: usize,
        noise_seed: Option<u64>,
    ) -> Result<Vec<f32>, String> {
        const HALO: usize = 24;
        if !codes.ok() || frame_hi > codes.frames() || frame_lo >= frame_hi {
            return Err("snac: bad frame range".into());
        }
        let (start, end) = (frame_lo * LATENTS_PER_FRAME, frame_hi * LATENTS_PER_FRAME);
        let lo = start.saturating_sub(HALO) & !3;
        let hi = (end + HALO).min(codes.l3.len());
        let mut rng = noise_seed.map(|s| Pcg::new(s ^ (frame_lo as u64).wrapping_mul(0x9e3779b97f4a7c15)));
        let audio = self.decode_range(codes, lo, hi, &mut rng);
        let head = (start - lo) * 512;
        let tail = (hi - end) * 512;
        Ok(audio[head..audio.len() - tail].to_vec())
    }
}

// The golden-vector validation against the official PyTorch implementation
// (SNR 52-59 dB with f16 weights; chunked and frame-stitched decode bit-exact
// vs full decode; ~2-3x realtime on one x86 core) needs the 26 MB weights file
// and lives in the model-volume workflow, not in this test suite. What CAN be
// asserted without weights is the machinery around the math.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_covers_the_cases_that_matter() {
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xbc00), -1.0);
        assert_eq!(f16_to_f32(0x3800), 0.5);
        assert_eq!(f16_to_f32(0x7bff), 65504.0); // f16 max
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24)); // smallest subnormal
        assert!(f16_to_f32(0x7c00).is_infinite());
        assert!(f16_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn the_container_rejects_what_it_should() {
        assert!(Decoder::from_bytes(b"NOTSNAC!").is_err());
        assert!(Decoder::from_bytes(b"SNACDEC1").is_err()); // truncated after magic
        // right magic, zero tensors: parses, but every lookup is missing
        let mut buf = b"SNACDEC1".to_vec();
        buf.extend_from_slice(&0u32.to_le_bytes());
        let e = Decoder::from_bytes(&buf).err().unwrap();
        assert!(e.contains("missing tensor"), "{e}");
    }

    #[test]
    fn code_streams_must_be_1_2_4() {
        let ok = Codes { l1: vec![0; 4], l2: vec![0; 8], l3: vec![0; 16] };
        assert!(ok.ok());
        assert_eq!(ok.frames(), 4);
        let bad = Codes { l1: vec![0; 4], l2: vec![0; 8], l3: vec![0; 15] };
        assert!(!bad.ok());
        let out_of_range = Codes { l1: vec![4096; 4], l2: vec![0; 8], l3: vec![0; 16] };
        assert!(!out_of_range.ok());
        let empty = Codes { l1: vec![], l2: vec![], l3: vec![] };
        assert!(!empty.ok());
    }

    #[test]
    fn the_noise_rng_is_deterministic_and_roughly_normal() {
        let mut a = Pcg::new(7);
        let mut b = Pcg::new(7);
        let xs: Vec<f32> = (0..10_000).map(|_| a.gauss()).collect();
        assert!((0..64).all(|i| xs[i] == b.gauss()));
        let mean: f32 = xs.iter().sum::<f32>() / xs.len() as f32;
        let var: f32 = xs.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / xs.len() as f32;
        assert!(mean.abs() < 0.05, "mean {mean}");
        assert!((var - 1.0).abs() < 0.1, "var {var}");
    }

    #[test]
    fn fast_cos_stays_within_8e6_of_libm() {
        let mut worst = 0.0f32;
        for i in -10_000..10_000 {
            let x = i as f32 * 0.01;
            worst = worst.max((fast_cos(x) - x.cos()).abs());
        }
        assert!(worst < 8e-6, "worst {worst}");
    }

    #[test]
    fn a_frame_is_2048_samples() {
        // the contract lib.rs and nn.rs stream by
        assert_eq!(FRAME_SAMPLES, LATENTS_PER_FRAME * 512);
        assert_eq!(SAMPLE_RATE, 24_000);
    }
}
