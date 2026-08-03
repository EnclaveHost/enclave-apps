//! Audio-token sampling, slot-constrained.
//!
//! At generation step g the model is supposed to emit a token from frame slot
//! g % 7, and each slot owns its own contiguous 4096-id range (maya.rs). The
//! sampler enforces that by CONSTRUCTION: it considers only the 4096 logits of
//! the current slot (plus the audio EOS, at a frame boundary once the minimum
//! length is met) and never looks at the rest of the 156,960-wide row. A text
//! token, a header token or a wrong-slot audio token is therefore not
//! low-probability - it is impossible, and every downstream stage (unpack,
//! decode) can treat its input as well-formed by contract.
//!
//! Temperature + top-p over the slot, repetition penalty over a recent window
//! of generated ids, straight from the Maya1 card's recommended settings. The
//! RNG is the same small xorshift the sibling apps use - sampling noise, not
//! cryptography.

use crate::maya::{CODEBOOK, CODE_EOS, FRAME_TOKENS, SNAC_MIN};

pub struct SampleParams {
    pub temperature: f32, // 0 = greedy
    pub top_p: f32,       // nucleus; 1.0 = off
    pub rep_penalty: f32, // 1.0 = off
    pub rep_window: usize, // 0 = the whole generation
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

/// Sample the next audio token. `logits` is the host's full dense row;
/// `generated` the audio ids sampled so far this episode (slot = len % 7);
/// `allow_eos` is lib-level policy (frame boundary AND minimum frames met).
/// Returns the sampled id - possibly CODE_EOS when allowed.
pub fn pick_audio_token(
    logits: &[f32],
    generated: &[u32],
    allow_eos: bool,
    p: &SampleParams,
    rng: &mut Rng,
) -> Result<u32, String> {
    let slot = generated.len() % FRAME_TOKENS;
    let base = (SNAC_MIN + slot as u32 * CODEBOOK) as usize;
    if logits.len() < base + CODEBOOK as usize {
        return Err(format!(
            "logits row is {} wide but slot {slot} needs ids up to {} - the volume under this \
             config is not a SNAC speech model",
            logits.len(),
            base + CODEBOOK as usize
        ));
    }

    // candidates: (id, logit) for the slot, plus EOS when policy allows
    let mut cand: Vec<(u32, f32)> = Vec::with_capacity(CODEBOOK as usize + 1);
    for j in 0..CODEBOOK as usize {
        cand.push(((base + j) as u32, logits[base + j]));
    }
    if allow_eos {
        cand.push((CODE_EOS, logits[CODE_EOS as usize]));
    }

    // repetition penalty over the recent window (ids outside it are unscathed;
    // only in-slot ids can match, which is exactly the set being sampled)
    if p.rep_penalty > 1.0 && !generated.is_empty() {
        let w = if p.rep_window == 0 { generated.len() } else { p.rep_window };
        let recent = &generated[generated.len().saturating_sub(w)..];
        for &t in recent {
            if (t as usize) >= base && (t as usize) < base + CODEBOOK as usize {
                let c = &mut cand[t as usize - base];
                if c.1 > 0.0 {
                    c.1 /= p.rep_penalty;
                } else {
                    c.1 *= p.rep_penalty;
                }
            }
        }
    }

    if p.temperature <= 0.0 {
        return Ok(cand
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|&(id, _)| id)
            .unwrap());
    }

    cand.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let max_l = cand[0].1;
    let mut probs: Vec<f32> =
        cand.iter().map(|&(_, v)| ((v - max_l) / p.temperature).exp()).collect();
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
            return Ok(cand[i].0);
        }
    }
    Ok(cand[0].0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(vocab: usize) -> Vec<f32> {
        vec![0.0; vocab]
    }
    const VOCAB: usize = 156_960;

    fn params(temp: f32) -> SampleParams {
        SampleParams { temperature: temp, top_p: 0.9, rep_penalty: 1.1, rep_window: 0 }
    }

    #[test]
    fn only_the_current_slot_can_win() {
        let mut logits = row(VOCAB);
        // make a slot-3 id the global argmax while we sample slot 0
        logits[(SNAC_MIN + 3 * CODEBOOK) as usize] = 100.0;
        logits[SNAC_MIN as usize + 7] = 1.0; // best in slot 0
        let id = pick_audio_token(&logits, &[], false, &params(0.0), &mut Rng::new(1)).unwrap();
        assert_eq!(id, SNAC_MIN + 7);
        // and with one token generated, slot 1's range is what is sampled
        let id = pick_audio_token(&logits, &[SNAC_MIN], false, &params(0.0), &mut Rng::new(1))
            .unwrap();
        assert!(id >= SNAC_MIN + CODEBOOK && id < SNAC_MIN + 2 * CODEBOOK);
    }

    #[test]
    fn eos_is_impossible_until_allowed_and_wins_when_dominant() {
        let mut logits = row(VOCAB);
        logits[CODE_EOS as usize] = 100.0;
        let id = pick_audio_token(&logits, &[], false, &params(0.0), &mut Rng::new(1)).unwrap();
        assert_ne!(id, CODE_EOS, "EOS sampled while disallowed");
        let id = pick_audio_token(&logits, &[], true, &params(0.0), &mut Rng::new(1)).unwrap();
        assert_eq!(id, CODE_EOS);
    }

    #[test]
    fn the_repetition_penalty_dethrones_a_repeated_id() {
        let mut logits = row(VOCAB);
        let a = SNAC_MIN + 5;
        let b = SNAC_MIN + 9;
        logits[a as usize] = 1.00;
        logits[b as usize] = 0.98;
        // greedy would pick `a`; after 7k generated steps of `a` (same slot 0
        // each full frame) the penalty flips the order
        let generated: Vec<u32> = (0..FRAME_TOKENS as u32).cycle().take(70)
            .map(|j| SNAC_MIN + j * CODEBOOK + if j == 0 { 5 } else { 0 })
            .collect();
        let p = SampleParams { temperature: 0.0, top_p: 1.0, rep_penalty: 1.1, rep_window: 0 };
        let id = pick_audio_token(&logits, &generated, false, &p, &mut Rng::new(1)).unwrap();
        assert_eq!(id, b);
        // with the penalty off, the repeat wins again
        let p0 = SampleParams { temperature: 0.0, top_p: 1.0, rep_penalty: 1.0, rep_window: 0 };
        let id = pick_audio_token(&logits, &generated, false, &p0, &mut Rng::new(1)).unwrap();
        assert_eq!(id, a);
    }

    #[test]
    fn sampling_stays_in_slot_at_temperature() {
        let logits = row(VOCAB); // uniform: any id in slot is fair game
        let mut rng = Rng::new(42);
        let mut generated: Vec<u32> = Vec::new();
        for _ in 0..70 {
            let id =
                pick_audio_token(&logits, &generated, false, &params(0.8), &mut rng).unwrap();
            let slot = (generated.len() % FRAME_TOKENS) as u32;
            let lo = SNAC_MIN + slot * CODEBOOK;
            assert!(id >= lo && id < lo + CODEBOOK, "id {id} escaped slot {slot}");
            generated.push(id);
        }
        // 70 tokens = 10 well-formed frames
        assert!(crate::maya::unpack_frames(&generated).is_ok());
    }

    #[test]
    fn a_short_row_is_an_error_not_a_panic() {
        let logits = row(1000);
        let e = pick_audio_token(&logits, &[], false, &params(0.4), &mut Rng::new(1));
        assert!(e.is_err());
    }
}
