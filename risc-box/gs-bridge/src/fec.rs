// Reed-Solomon over GF(2^8), wire-compatible with nanors — the FEC library
// moonlight-common-c uses to recover dropped video shards.
//
// The client rebuilds missing shards with nanors' parity matrix, so ours must
// be bit-identical. From nanors/rs.c reed_solomon_new_static():
//
//     for (int j = 0; j < rs->ps; j++)
//         for (int i = 0; i < rs->ds; i++)
//             p[j][i] = GF2_8_INV[(rs->ps + i) ^ j];
//
// a Cauchy matrix over GF(2^8) with the polynomial nanors generated its tables
// from: 285 (0x11D), per deps/obl/gf2_8_tables.h.
//
// Encoding is nanors' gemm(): parity[j] = sum over i of p[j][i] * data[i].
// We only ever encode (the client does all the decoding), so that's all this is.

const POLY: u16 = 0x11D;

/// GF(2^8) log/exp/inverse tables for polynomial 0x11D, built once at startup.
struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
    inv: [u8; 256],
}

impl Gf {
    fn new() -> Gf {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= POLY;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }

        let mut inv = [0u8; 256];
        // inv[0] is 0 by convention (GF2_8_INV[0] == 0 in nanors' table).
        for a in 1..256usize {
            inv[a] = exp[255 - log[a] as usize];
        }

        Gf { exp, log, inv }
    }

    #[inline]
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            return 0;
        }
        self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
    }
}

static GF: std::sync::LazyLock<Gf> = std::sync::LazyLock::new(Gf::new);

/// GameStream audio uses a fixed OpenFEC matrix, distinct from video nanors.
/// See moonlight-common-c/src/RtpAudioQueue.c, RtpaInitializeQueue().
pub fn encode_audio(data: &[&[u8]]) -> Vec<Vec<u8>> {
    const MATRIX: [[u8; 4]; 2] = [
        [0x77, 0x40, 0x38, 0x0e],
        [0xc7, 0xa7, 0x0d, 0x6c],
    ];
    assert_eq!(data.len(), 4);
    let len = data[0].len();
    assert!(data.iter().all(|s| s.len() == len));
    MATRIX.iter().map(|row| {
        let mut out = vec![0; len];
        for (&coeff, shard) in row.iter().zip(data) {
            for (o, &s) in out.iter_mut().zip(*shard) {
                *o ^= GF.mul(coeff, s);
            }
        }
        out
    }).collect()
}

/// The most shards (data + parity) a single FEC block can hold. nanors caps
/// this at 255 (DATA_SHARDS_MAX); Sunshine sizes its blocks to respect it.
pub const DATA_SHARDS_MAX: usize = 255;

/// Generate `parity_count` parity shards from `data`, which must be
/// `data.len()` equal-sized shards. Returns the parity shards.
pub fn encode(data: &[&[u8]], parity_count: usize) -> Vec<Vec<u8>> {
    let gf = &*GF;
    let ds = data.len();
    let shard_len = data.first().map(|s| s.len()).unwrap_or(0);
    debug_assert!(data.iter().all(|s| s.len() == shard_len));

    let mut parity = vec![vec![0u8; shard_len]; parity_count];

    for (j, out) in parity.iter_mut().enumerate() {
        for (i, shard) in data.iter().enumerate() {
            // The Cauchy coefficient nanors uses for this (parity, data) pair.
            let coeff = gf.inv[((parity_count + i) ^ j) & 0xFF];
            if coeff == 0 {
                continue;
            }
            if coeff == 1 {
                for (o, &s) in out.iter_mut().zip(shard.iter()) {
                    *o ^= s;
                }
            } else {
                let log_c = gf.log[coeff as usize] as usize;
                for (o, &s) in out.iter_mut().zip(shard.iter()) {
                    if s != 0 {
                        *o ^= gf.exp[log_c + gf.log[s as usize] as usize];
                    }
                }
            }
        }
    }

    parity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_matches_nanors_tables() {
        let gf = &*GF;
        // Spot-checks against deps/obl/gf2_8_tables.h GF2_8_INV.
        assert_eq!(gf.inv[0], 0);
        assert_eq!(gf.inv[1], 1);
        assert_eq!(gf.inv[2], 142);
        assert_eq!(gf.inv[3], 244);
        assert_eq!(gf.inv[4], 71);
        assert_eq!(gf.inv[16], 216);
        assert_eq!(gf.inv[255], 253);
        // An inverse is an inverse.
        for a in 1..256usize {
            assert_eq!(gf.mul(a as u8, gf.inv[a]), 1);
        }
    }

    /// Reproduce nanors' decode against our encode: drop shards, rebuild them
    /// by inverting the same Cauchy system, and check we get the data back.
    /// This is the property the client depends on.
    #[test]
    fn parity_recovers_dropped_data_shards() {
        let gf = &*GF;
        let ds = 6usize;
        let ps = 3usize;
        let len = 64usize;

        let data: Vec<Vec<u8>> = (0..ds)
            .map(|i| (0..len).map(|j| ((i * 31 + j * 7 + 11) % 256) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
        let parity = encode(&refs, ps);

        // Drop data shards 1, 4 and recover them from parity rows 0, 1.
        let lost = [1usize, 4usize];
        let present: Vec<usize> = (0..ds).filter(|i| !lost.contains(i)).collect();

        // For each parity row we use, subtract the contribution of the shards
        // we still have; what's left is a linear system in the lost shards.
        let rows = [0usize, 1usize];
        let mut rhs: Vec<Vec<u8>> = rows
            .iter()
            .map(|&j| {
                let mut acc = parity[j].clone();
                for &i in &present {
                    let c = gf.inv[((ps + i) ^ j) & 0xFF];
                    for (a, &b) in acc.iter_mut().zip(data[i].iter()) {
                        *a ^= gf.mul(c, b);
                    }
                }
                acc
            })
            .collect();

        // 2x2 solve over GF(2^8).
        let mut m = [[0u8; 2]; 2];
        for (r, &j) in rows.iter().enumerate() {
            for (c, &i) in lost.iter().enumerate() {
                m[r][c] = gf.inv[((ps + i) ^ j) & 0xFF];
            }
        }
        // Eliminate.
        let f = gf.mul(m[1][0], gf.inv[m[0][0] as usize]);
        for k in 0..2 {
            m[1][k] ^= gf.mul(f, m[0][k]);
        }
        let (first, rest) = rhs.split_at_mut(1);
        for (a, &b) in rest[0].iter_mut().zip(first[0].iter()) {
            *a ^= gf.mul(f, b);
        }
        let inv11 = gf.inv[m[1][1] as usize];
        let x1: Vec<u8> = rhs[1].iter().map(|&v| gf.mul(inv11, v)).collect();
        let inv00 = gf.inv[m[0][0] as usize];
        let x0: Vec<u8> = rhs[0]
            .iter()
            .zip(x1.iter())
            .map(|(&v, &b)| gf.mul(inv00, v ^ gf.mul(m[0][1], b)))
            .collect();

        assert_eq!(x0, data[1], "recovered shard 1 should match the original");
        assert_eq!(x1, data[4], "recovered shard 4 should match the original");
    }
}
