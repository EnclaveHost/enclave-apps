//! Token sampling: greedy at temperature 0, otherwise temperature +
//! top-k/top-p over softmaxed logits, with the repetition penalty applied
//! first. The RNG is a small xorshift seeded per-request from the clock -
//! sampling noise, not cryptography (the platform's wasi:random stays for
//! things that matter).

pub struct SampleParams {
    pub temperature: f32, // 0 = greedy
    pub top_p: f32,       // nucleus; 1.0 = off
    pub top_k: usize,     // 0 = off
    pub rep_penalty: f32,
    pub rep_window: usize,
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_f32(&mut self) -> f32 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let v = x.wrapping_mul(0x2545F4914F6CDD1D) >> 40;
        (v as f32) / ((1u64 << 24) as f32)
    }
}

pub fn pick_token(logits: &mut [f32], recent: &[u32], p: &SampleParams, rng: &mut Rng) -> u32 {
    for &t in recent {
        if let Some(l) = logits.get_mut(t as usize) {
            if *l > 0.0 {
                *l /= p.rep_penalty;
            } else {
                *l *= p.rep_penalty;
            }
        }
    }
    if p.temperature <= 0.0 {
        let mut best = 0usize;
        let mut best_v = f32::NEG_INFINITY;
        for (i, &v) in logits.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best = i;
            }
        }
        return best as u32;
    }
    // temperature + top-k prefilter: sort a bounded candidate set instead of
    // the whole vocab (151936 floats) - top 256 covers any sane top_p mass
    let k = if p.top_k > 0 { p.top_k.min(256) } else { 256 };
    let mut cand: Vec<(usize, f32)> = Vec::with_capacity(k + 1);
    let mut min_in = f32::NEG_INFINITY;
    for (i, &v) in logits.iter().enumerate() {
        if v > min_in || cand.len() < k {
            cand.push((i, v));
            if cand.len() > k {
                // drop the current minimum
                let (mi, _) = cand
                    .iter()
                    .enumerate()
                    .min_by(|a, b| a.1 .1.partial_cmp(&b.1 .1).unwrap())
                    .map(|(idx, &(i2, v2))| (idx, (i2, v2)))
                    .unwrap();
                cand.swap_remove(mi);
                min_in = cand
                    .iter()
                    .map(|&(_, v2)| v2)
                    .fold(f32::INFINITY, f32::min);
            }
        }
    }
    cand.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    // softmax at temperature over the candidates
    let max_l = cand[0].1;
    let mut probs: Vec<f32> = cand
        .iter()
        .map(|&(_, v)| ((v - max_l) / p.temperature).exp())
        .collect();
    let sum: f32 = probs.iter().sum();
    for q in probs.iter_mut() {
        *q /= sum;
    }
    // nucleus cut
    let mut cut = probs.len();
    if p.top_p < 1.0 {
        let mut acc = 0.0;
        for (i, &q) in probs.iter().enumerate() {
            acc += q;
            if acc >= p.top_p {
                cut = i + 1;
                break;
            }
        }
    }
    let mass: f32 = probs[..cut].iter().sum();
    let mut r = rng.next_f32() * mass;
    for i in 0..cut {
        r -= probs[i];
        if r <= 0.0 {
            return cand[i].0 as u32;
        }
    }
    cand[0].0 as u32
}

/// A logits row as the host returned it: dense (full vocab, index = token
/// id) or sparse (the host's top-K - ids + values, rows sorted descending).
/// Sparse rows exist because shipping a full 248320-float row per position
/// across the wasi-nn boundary - and scanning it in wasm - was most of what
/// made a speculative verify round cost 5-6 plain decode steps.
pub struct Row {
    pub ids: Option<Vec<u32>>,
    pub vals: Vec<f32>,
}

impl Row {
    pub fn dense(vals: Vec<f32>) -> Row {
        Row { ids: None, vals }
    }
}

/// pick_token over either row shape.
pub fn pick_row(row: &mut Row, recent: &[u32], p: &SampleParams, rng: &mut Rng) -> u32 {
    match &row.ids {
        None => pick_token(&mut row.vals, recent, p, rng),
        Some(ids) => {
            let ids = ids.clone();
            pick_sparse(&ids, &mut row.vals, recent, p, rng)
        }
    }
}

/// The sparse twin of pick_token: the host already did the top-K selection,
/// so what remains is the repetition penalty (by membership now, not by
/// index), the guest's own top_k bound, and the softmax/nucleus sample -
/// all over K entries instead of the vocabulary.
fn pick_sparse(ids: &[u32], vals: &mut [f32], recent: &[u32], p: &SampleParams, rng: &mut Rng) -> u32 {
    for (i, &id) in ids.iter().enumerate() {
        if recent.contains(&id) {
            if vals[i] > 0.0 {
                vals[i] /= p.rep_penalty;
            } else {
                vals[i] *= p.rep_penalty;
            }
        }
    }
    if p.temperature <= 0.0 {
        let mut best = 0usize;
        let mut best_v = f32::NEG_INFINITY;
        for (i, &v) in vals.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best = i;
            }
        }
        return ids[best];
    }
    let k = if p.top_k > 0 { p.top_k.min(ids.len()) } else { ids.len() };
    let mut cand: Vec<(u32, f32)> = ids.iter().copied().zip(vals.iter().copied()).collect();
    cand.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    cand.truncate(k.max(1));
    let max_l = cand[0].1;
    let mut probs: Vec<f32> = cand
        .iter()
        .map(|&(_, v)| ((v - max_l) / p.temperature).exp())
        .collect();
    let sum: f32 = probs.iter().sum();
    for q in probs.iter_mut() {
        *q /= sum;
    }
    let mut cut = probs.len();
    if p.top_p < 1.0 {
        let mut acc = 0.0;
        for (i, &q) in probs.iter().enumerate() {
            acc += q;
            if acc >= p.top_p {
                cut = i + 1;
                break;
            }
        }
    }
    let mass: f32 = probs[..cut].iter().sum();
    let mut r = rng.next_f32() * mass;
    for i in 0..cut {
        r -= probs[i];
        if r <= 0.0 {
            return cand[i].0;
        }
    }
    cand[0].0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(temp: f32, top_k: usize, rep: f32) -> SampleParams {
        SampleParams { temperature: temp, top_p: 1.0, top_k, rep_penalty: rep, rep_window: 64 }
    }

    /// temp-0 sparse pick = the id with the highest value, exactly like the
    /// dense argmax over a row whose top-K these candidates are
    #[test]
    fn sparse_greedy_matches_dense() {
        let mut dense = vec![0.0f32; 100];
        dense[7] = 5.0;
        dense[42] = 9.0;
        dense[93] = 3.0;
        let mut rng = Rng::new(1);
        let d = pick_token(&mut dense.clone(), &[], &params(0.0, 0, 1.0), &mut rng);
        let mut row = Row { ids: Some(vec![42, 7, 93]), vals: vec![9.0, 5.0, 3.0] };
        let s = pick_row(&mut row, &[], &params(0.0, 0, 1.0), &mut rng);
        assert_eq!(d, s);
        assert_eq!(s, 42);
    }

    /// the repetition penalty applies by MEMBERSHIP in sparse rows: a recent
    /// top candidate can be demoted below the runner-up
    #[test]
    fn sparse_penalty_by_membership() {
        let mut row = Row { ids: Some(vec![10, 20]), vals: vec![2.0, 1.9] };
        let mut rng = Rng::new(1);
        let picked = pick_row(&mut row, &[10], &params(0.0, 0, 1.2), &mut rng);
        assert_eq!(picked, 20); // 2.0/1.2 = 1.67 < 1.9
    }

    /// negative logits are suppressed by the penalty in both directions
    #[test]
    fn sparse_penalty_negative_suppresses() {
        let mut row = Row { ids: Some(vec![10, 20]), vals: vec![-1.0, -1.1] };
        let mut rng = Rng::new(1);
        let picked = pick_row(&mut row, &[10], &params(0.0, 0, 1.2), &mut rng);
        assert_eq!(picked, 20); // -1.0*1.2 = -1.2 < -1.1
    }

    /// at temperature, top_k bounds the candidate set even within the
    /// host's top-256 (a peaked distribution stays on its winner)
    #[test]
    fn sparse_top_k_truncates() {
        let mut row = Row { ids: Some(vec![1, 2, 3, 4]), vals: vec![100.0, 1.0, 0.5, 0.1] };
        let mut rng = Rng::new(42);
        let picked = pick_row(&mut row, &[], &params(0.7, 1, 1.0), &mut rng);
        assert_eq!(picked, 1); // top_k=1 = greedy on the max
    }
}
