//! Procedural world: deterministic rolling grass hills, regenerated on demand
//! (the platform gives apps no persistent disk, so nothing is ever saved —
//! the world is a pure function of the seed plus an in-memory edit log that
//! lives exactly as long as the deployment).
//!
//! Chunks are serialized in the 1.8 (protocol 47) format: per included
//! section 4096 *little-endian* u16 block states (id<<4 | meta), then 2048
//! bytes block light per section, 2048 bytes sky light per section, then 256
//! biome bytes. We light everything fully and pin time at noon.

use crate::proto::P;
use std::collections::HashMap;

const SEED: u64 = 0x6e616e6d63_2026; // historic seed (changing it regenerates the world)

pub const AIR: u16 = 0;
pub const STONE: u16 = 1 << 4;
pub const GRASS: u16 = 2 << 4;
pub const DIRT: u16 = 3 << 4;
pub const BEDROCK: u16 = 7 << 4;
pub const TALL_GRASS: u16 = (31 << 4) | 1;
pub const DANDELION: u16 = 37 << 4;
pub const POPPY: u16 = 38 << 4;

pub struct World {
    /// Player edits: world (x,y,z) → block state. The only mutable state.
    edits: HashMap<(i32, i32, i32), u16>,
    /// Serialized-chunk cache so N players near spawn don't re-serialize the
    /// same chunks. Bounded; wiped wholesale when full (regeneration is cheap).
    cache: HashMap<(i32, i32), Vec<u8>>,
}

fn hash2(x: i64, z: i64) -> u64 {
    let mut h = (x as u64).wrapping_mul(0x9E3779B97F4A7C15)
        ^ (z as u64).wrapping_mul(0xC2B2AE3D27D4EB4F)
        ^ SEED;
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51AFD7ED558CCD);
    h ^= h >> 33;
    h
}

/// Value noise in [0,1): bilinear blend of lattice hashes with smoothstep.
fn noise(x: f64, z: f64, salt: i64) -> f64 {
    let xf = x.floor();
    let zf = z.floor();
    let (x0, z0) = (xf as i64, zf as i64);
    let corner = |ix: i64, iz: i64| (hash2(ix ^ salt.wrapping_mul(0x51ED), iz) >> 11) as f64 / (1u64 << 53) as f64;
    let sx = {
        let t = x - xf;
        t * t * (3.0 - 2.0 * t)
    };
    let sz = {
        let t = z - zf;
        t * t * (3.0 - 2.0 * t)
    };
    let top = corner(x0, z0) * (1.0 - sx) + corner(x0 + 1, z0) * sx;
    let bot = corner(x0, z0 + 1) * (1.0 - sx) + corner(x0 + 1, z0 + 1) * sx;
    top * (1.0 - sz) + bot * sz
}

/// Terrain height (the y of the grass block) at a world column.
pub fn height(x: i32, z: i32) -> i32 {
    let (xf, zf) = (x as f64, z as f64);
    let h = 64.0
        + (noise(xf / 48.0, zf / 48.0, 1) - 0.5) * 22.0
        + (noise(xf / 12.0, zf / 12.0, 2) - 0.5) * 5.0;
    (h as i32).clamp(8, 120)
}

/// The generated (pre-edit) block at a world position.
fn generated(x: i32, y: i32, z: i32) -> u16 {
    if !(0..256).contains(&y) {
        return AIR;
    }
    let h = height(x, z);
    if y == 0 {
        BEDROCK
    } else if y <= h - 4 {
        STONE
    } else if y < h {
        DIRT
    } else if y == h {
        GRASS
    } else if y == h + 1 {
        // Sparse decoration on the surface.
        match hash2(x as i64, z as i64) % 61 {
            0..=4 => TALL_GRASS,
            5 => DANDELION,
            6 => POPPY,
            _ => AIR,
        }
    } else {
        AIR
    }
}

impl World {
    pub fn new() -> Self {
        World { edits: HashMap::new(), cache: HashMap::new() }
    }

    pub fn spawn() -> (f64, f64, f64) {
        let h = height(8, 8);
        (8.5, (h + 1) as f64, 8.5)
    }

    pub fn block_at(&self, x: i32, y: i32, z: i32) -> u16 {
        match self.edits.get(&(x, y, z)) {
            Some(&s) => s,
            None => generated(x, y, z),
        }
    }

    pub fn set_block(&mut self, x: i32, y: i32, z: i32, state: u16) {
        if !(0..256).contains(&y) {
            return;
        }
        if generated(x, y, z) == state {
            self.edits.remove(&(x, y, z));
        } else {
            self.edits.insert((x, y, z), state);
        }
        self.cache.remove(&(x.div_euclid(16), z.div_euclid(16)));
    }

    /// Full 0x21 Chunk Data frame (ground-up continuous) for chunk (cx,cz).
    pub fn chunk_frame(&mut self, cx: i32, cz: i32) -> Vec<u8> {
        if let Some(f) = self.cache.get(&(cx, cz)) {
            return f.clone();
        }

        // Column heights, and the topmost non-air y (edits included) to size
        // the section mask.
        let (bx, bz) = (cx * 16, cz * 16);
        let mut heights = [[0i32; 16]; 16];
        let mut top = 0i32;
        for z in 0..16usize {
            for x in 0..16usize {
                let h = height(bx + x as i32, bz + z as i32);
                heights[z][x] = h;
                top = top.max(h + 1); // +1 for surface decoration
            }
        }
        for (&(_, y, _), &s) in self.edits.iter().filter(|(&(x, _, z), _)| {
            x.div_euclid(16) == cx && z.div_euclid(16) == cz
        }) {
            if s != AIR {
                top = top.max(y);
            }
        }
        let nsec = ((top >> 4) + 1).clamp(1, 16) as usize;
        let mask: u16 = ((1u32 << nsec) - 1) as u16;

        let mut data = Vec::with_capacity(nsec * (8192 + 4096) + 256);
        for s in 0..nsec {
            for y in 0..16usize {
                let wy = (s * 16 + y) as i32;
                for z in 0..16usize {
                    for x in 0..16usize {
                        let state = match self.edits.get(&(bx + x as i32, wy, bz + z as i32)) {
                            Some(&e) => e,
                            None => {
                                let h = heights[z][x];
                                if wy == 0 {
                                    BEDROCK
                                } else if wy <= h - 4 {
                                    STONE
                                } else if wy < h {
                                    DIRT
                                } else if wy == h {
                                    GRASS
                                } else if wy == h + 1 {
                                    generated(bx + x as i32, wy, bz + z as i32)
                                } else {
                                    AIR
                                }
                            }
                        };
                        data.extend_from_slice(&state.to_le_bytes());
                    }
                }
            }
        }
        data.resize(data.len() + nsec * 2048, 0x00); // block light: none
        data.resize(data.len() + nsec * 2048, 0xFF); // sky light: full
        data.resize(data.len() + 256, 1); // biome: plains

        let mut p = P::new(0x21);
        p.i32(cx).i32(cz).bool(true).u16(mask).varint(data.len() as i32).raw(&data);
        let frame = p.frame();

        if self.cache.len() >= 256 {
            self.cache.clear();
        }
        self.cache.insert((cx, cz), frame.clone());
        frame
    }

    /// 1.8 chunk unload: ground-up continuous with an empty section mask.
    pub fn unload_frame(cx: i32, cz: i32) -> Vec<u8> {
        let mut p = P::new(0x21);
        p.i32(cx).i32(cz).bool(true).u16(0).varint(0);
        p.frame()
    }
}
