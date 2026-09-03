//! Text to vector, inside the component: a static-embedding model that runs
//! as table lookups.
//!
//! The platform's inference engine offers tenants no embeddings verb, and a
//! wasip2 component has no GPU and (here) no wasi-nn, so the model is one
//! that needs neither: minishlab/potion-base-8M (MIT), a Model2Vec
//! distillation of bge-base-en-v1.5. It is a WordPiece vocabulary of 29,528
//! tokens, one 256-dimensional vector each; a text's embedding is the MEAN of
//! its tokens' vectors, L2-normalised. That is the whole forward pass, so a
//! note embeds in microseconds and the query side costs nothing measurable.
//! The table is embedded in the wasm as int8 rows with one scale each
//! (scripts/fetch-model.sh converts it, pinned by revision and digest).
//!
//! The tokenizer is BERT's, implemented here rather than pulled in: clean
//! text, lowercase and strip accents, split punctuation and CJK characters,
//! then greedy longest-match WordPiece with `##` continuations and [UNK] for
//! a word no piece covers. Query and notes go through the same code, so the
//! two sides agree with each other exactly; agreement with the reference
//! tokenizer is what the tests pin on ordinary English and the edge cases.

use std::collections::HashMap;

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

static TABLE: &[u8] = include_bytes!("../model/embeddings.i8");
static VOCAB: &str = include_str!("../model/vocab.txt");
pub const MODEL: &str = "potion-base-8M";
const UNK: &str = "[UNK]";
const MAX_WORD_CHARS: usize = 100;

pub struct Embedder {
    vocab: HashMap<&'static str, u32>,
    unk: u32,
    dim: usize,
}

impl Embedder {
    /// Parse the embedded table header and index the vocabulary. Cheap
    /// enough to do per request (tens of thousands of map inserts).
    pub fn new() -> Result<Embedder, String> {
        if TABLE.len() < 13 || &TABLE[..5] != b"JOTV1" {
            return Err("embedding table has the wrong magic".into());
        }
        let n = u32::from_le_bytes(TABLE[5..9].try_into().unwrap()) as usize;
        let dim = u32::from_le_bytes(TABLE[9..13].try_into().unwrap()) as usize;
        if TABLE.len() != 13 + n * (4 + dim) {
            return Err("embedding table is truncated".into());
        }
        let mut vocab = HashMap::with_capacity(n);
        for (i, tok) in VOCAB.lines().enumerate() {
            vocab.insert(tok, i as u32);
        }
        if vocab.len() != n {
            return Err(format!("vocab has {} tokens, table has {n} rows", vocab.len()));
        }
        let unk = *vocab.get(UNK).ok_or("vocab has no [UNK]")?;
        Ok(Embedder { vocab, unk, dim })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    fn row(&self, id: u32) -> (f32, &'static [u8]) {
        let start = 13 + (id as usize) * (4 + self.dim);
        let scale = f32::from_le_bytes(TABLE[start..start + 4].try_into().unwrap());
        (scale, &TABLE[start + 4..start + 4 + self.dim])
    }

    /// Token ids, no special tokens: what Model2Vec pools.
    pub fn tokenize(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        for word in basic_tokenize(text) {
            self.wordpiece(&word, &mut ids);
        }
        ids
    }

    fn wordpiece(&self, word: &str, out: &mut Vec<u32>) {
        if word.chars().count() > MAX_WORD_CHARS {
            out.push(self.unk);
            return;
        }
        let mut pieces = Vec::new();
        let mut start = 0;
        while start < word.len() {
            let mut end = word.len();
            let mut found = None;
            while start < end {
                let sub = &word[start..end];
                let cand = if start == 0 { sub.to_string() } else { format!("##{sub}") };
                if let Some(&id) = self.vocab.get(cand.as_str()) {
                    found = Some(id);
                    break;
                }
                // step back one CHARACTER, not one byte
                end = word[..end].char_indices().last().map(|(i, _)| i).unwrap_or(start);
            }
            match found {
                Some(id) => {
                    pieces.push(id);
                    start = end;
                }
                None => {
                    out.push(self.unk);
                    return;
                }
            }
        }
        out.extend(pieces);
    }

    /// Mean of the token vectors, L2-normalised. An empty text embeds to the
    /// zero vector, which scores zero against everything.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let ids = self.tokenize(text);
        let mut acc = vec![0f32; self.dim];
        if ids.is_empty() {
            return acc;
        }
        for id in &ids {
            let (scale, q) = self.row(*id);
            for (a, &b) in acc.iter_mut().zip(q) {
                *a += (b as i8) as f32 * scale;
            }
        }
        let inv = 1.0 / ids.len() as f32;
        for a in acc.iter_mut() {
            *a *= inv;
        }
        normalize(&mut acc);
        acc
    }

    #[cfg(test)]
    pub fn vocab_len(&self) -> usize {
        self.vocab.len()
    }
}

pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// BERT's basic tokenizer: control characters dropped, whitespace
/// collapsed, CJK characters isolated, lowercase, accents stripped (NFD then
/// drop combining marks), punctuation split into single-character tokens.
fn basic_tokenize(text: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\0' || c == '\u{fffd}' || (is_control(c) && !c.is_whitespace()) {
            continue;
        }
        if is_cjk(c) {
            cleaned.push(' ');
            cleaned.push(c);
            cleaned.push(' ');
        } else if c.is_whitespace() {
            cleaned.push(' ');
        } else {
            cleaned.push(c);
        }
    }
    let mut out = Vec::new();
    for word in cleaned.split(' ') {
        if word.is_empty() {
            continue;
        }
        let lowered: String = word
            .to_lowercase()
            .nfd()
            .filter(|c| !is_combining_mark(*c))
            .collect();
        let mut cur = String::new();
        for c in lowered.chars() {
            if is_punct(c) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn is_control(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') == false && (c.is_control() || matches!(c, '\u{200b}'..='\u{200f}'))
}

/// BERT's `_is_punctuation`: ASCII non-alphanumeric printables plus Unicode
/// punctuation. The general category is approximated by the punctuation
/// blocks that occur in practice.
fn is_punct(c: char) -> bool {
    let cp = c as u32;
    (33..=47).contains(&cp)
        || (58..=64).contains(&cp)
        || (91..=96).contains(&cp)
        || (123..=126).contains(&cp)
        || (0x2000..=0x206f).contains(&cp)
        || (0x2e00..=0x2e7f).contains(&cp)
        || (0x3000..=0x303f).contains(&cp)
        || (0xff00..=0xff0f).contains(&cp)
        || (0xff1a..=0xff20).contains(&cp)
        || (0xff3b..=0xff40).contains(&cp)
        || (0xff5b..=0xff65).contains(&cp)
        || matches!(cp, 0xa1..=0xbf if !c.is_alphanumeric())
}

/// BERT's `_is_chinese_char` ranges.
fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x4e00..=0x9fff).contains(&cp)
        || (0x3400..=0x4dbf).contains(&cp)
        || (0x20000..=0x2a6df).contains(&cp)
        || (0x2a700..=0x2b73f).contains(&cp)
        || (0x2b740..=0x2b81f).contains(&cp)
        || (0x2b820..=0x2ceaf).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0x2f800..=0x2fa1f).contains(&cp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(e: &Embedder, s: &str) -> Vec<String> {
        let by_id: Vec<&str> = VOCAB.lines().collect();
        e.tokenize(s).iter().map(|&i| by_id[i as usize].to_string()).collect()
    }

    #[test]
    fn table_and_vocab_agree() {
        let e = Embedder::new().unwrap();
        assert_eq!(e.dim(), 256);
        assert_eq!(e.vocab_len(), 29528);
    }

    #[test]
    fn wordpiece_matches_bert_on_the_usual_cases() {
        let e = Embedder::new().unwrap();
        // the classic reference example, plus accents, punctuation, and a
        // word no piece covers
        assert_eq!(pieces(&e, "unaffable"), ["una", "##ffa", "##ble"]);
        assert_eq!(pieces(&e, "Hello, World!"), ["hello", ",", "world", "!"]);
        assert_eq!(pieces(&e, "Résumé café"), ["resume", "cafe"]);
        // pinned against the reference tokenizer (tokenizers 0.23, this vocab)
        assert_eq!(pieces(&e, "IPv6-only egress"), ["ip", "##v", "##6", "-", "only", "e", "##gre", "##ss"]);
        assert_eq!(pieces(&e, "the fleet's outbound traffic"), ["the", "fleet", "'", "s", "out", "##bound", "traffic"]);
        assert_eq!(pieces(&e, "AAAA records"), ["aaa", "##a", "records"]);
        assert_eq!(pieces(&e, "R2 publishes AAAA records, so the bucket is reachable."),
                   ["r", "##2", "publishes", "aaa", "##a", "records", ",", "so", "the", "bucket", "is", "reach", "##able", "."]);
        assert_eq!(pieces(&e, "\u{1F600}"), ["[UNK]"]);
        assert!(pieces(&e, "").is_empty());
    }

    #[test]
    fn embeddings_are_unit_and_semantic() {
        let e = Embedder::new().unwrap();
        let cos = |a: &str, b: &str| -> f32 {
            e.embed(a).iter().zip(e.embed(b)).map(|(x, y)| x * y).sum()
        };
        let n: f32 = e.embed("the fleet's outbound traffic").iter().map(|x| x * x).sum();
        assert!((n - 1.0).abs() < 1e-4, "unit norm, got {n}");
        assert!(e.embed("").iter().all(|x| *x == 0.0));
        // near things score above far things; the margin is the model's,
        // not a tuning here
        let near = cos("network connectivity problems", "the outbound traffic cannot reach the host over ipv6");
        let far = cos("network connectivity problems", "grocery list: eggs, milk and bread");
        assert!(near > far + 0.1, "near {near} far {far}");
    }
}
