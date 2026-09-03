//! The search index: one sealed object per notebook holding every note's
//! chunks with their vectors, queried by BM25 and by cosine and fused.
//!
//! Why an object and not a database: a wasip2 component keeps nothing between
//! requests and reaches nothing but the bucket, so the "vector database" is a
//! JSON document under the caller's own prefix (`.jot/index.v1`), sealed like
//! the notes when the deployment has a master key. Read once per search, it
//! carries, per chunk: the note, the start line, the text, and the embedding
//! (int8 with one scale). BM25 statistics are recomputed from the chunk text
//! at query time, which keeps the document small and the format simple; at a
//! few thousand chunks that is milliseconds.
//!
//! Consistency: the index is updated on every write, append and delete, and
//! RECONCILED on every search against a fresh listing of the namespace: a
//! note whose ETag the index does not know is (re)indexed, a note that is
//! gone is dropped. So a note written outside the app, or a write whose index
//! update lost a race, is picked up by the next search; the work is bounded
//! per search and the response says how much is still pending.
//!
//! Fusion: BM25 ranks chunks by exact term overlap (the query said "AAAA",
//! find the note that says AAAA); the vectors rank them by meaning ("network
//! reachability" finds the note about egress and IPv6). Reciprocal rank
//! fusion joins the two lists without having to make their scores
//! commensurable, and the best chunk per note is what comes back.

use std::collections::{BTreeMap, HashMap, HashSet};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::embed::Embedder;

pub const INDEX_NAME: &str = ".jot/index.v1";
pub const VERSION: u32 = 1;
/// chunks per note: a long note is indexed by its first ~40 KB of text
pub const MAX_CHUNKS_PER_NOTE: usize = 48;
/// chunks per notebook, which bounds the index object at a few MB
pub const MAX_CHUNKS: usize = 4000;
/// a note bigger than this is not indexed at all (the bound on one fetch)
pub const MAX_NOTE_BYTES: u64 = 256 * 1024;
const CHUNK_TARGET: usize = 800;
const CHUNK_HARD: usize = 1200;
const SNIPPET_CHARS: usize = 400;
const RRF_K: f32 = 60.0;
const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

#[derive(Serialize, Deserialize, Clone)]
pub struct Chunk {
    pub line: usize,
    pub text: String,
    /// base64 of int8 components; `scale` dequantises them
    pub vec: String,
    pub scale: f32,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct NoteEntry {
    pub etag: String,
    pub chunks: Vec<Chunk>,
    /// the note had more text than MAX_CHUNKS_PER_NOTE covers
    #[serde(default)]
    pub partial: bool,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Index {
    pub v: u32,
    pub model: String,
    pub dim: usize,
    pub notes: BTreeMap<String, NoteEntry>,
}

impl Index {
    pub fn empty(model: &str, dim: usize) -> Index {
        Index { v: VERSION, model: model.to_string(), dim, notes: BTreeMap::new() }
    }

    /// Parse a stored index; a document from another model or version is
    /// treated as empty so it is rebuilt rather than mixed.
    pub fn parse(bytes: &[u8], model: &str, dim: usize) -> Index {
        match serde_json::from_slice::<Index>(bytes) {
            Ok(i) if i.v == VERSION && i.model == model && i.dim == dim => i,
            _ => Index::empty(model, dim),
        }
    }

    pub fn chunk_count(&self) -> usize {
        self.notes.values().map(|n| n.chunks.len()).sum()
    }

    /// What a fresh listing says about the index: notes to (re)index and
    /// notes to drop. `live` is (name, etag, size) for every object under
    /// the namespace that is a note.
    pub fn reconcile(&self, live: &[(String, String, u64)]) -> (Vec<(String, String, u64)>, Vec<String>) {
        let live_names: HashSet<&str> = live.iter().map(|(n, _, _)| n.as_str()).collect();
        let stale: Vec<(String, String, u64)> = live
            .iter()
            .filter(|(n, etag, _)| self.notes.get(n).map_or(true, |e| &e.etag != etag))
            .cloned()
            .collect();
        let gone: Vec<String> = self.notes.keys().filter(|n| !live_names.contains(n.as_str())).cloned().collect();
        (stale, gone)
    }

    /// Replace one note's entry from its text.
    pub fn put_note(&mut self, name: &str, etag: &str, text: &str, e: &Embedder) {
        let (pieces, partial) = chunk(text);
        let chunks = pieces
            .into_iter()
            .map(|(line, body)| {
                let v = e.embed(&format!("{name}\n{body}"));
                let (q, scale) = quantize(&v);
                Chunk { line, text: body, vec: B64.encode(q), scale }
            })
            .collect();
        self.notes.insert(name.to_string(), NoteEntry { etag: etag.to_string(), chunks, partial });
    }

    /// Record a note that is known but not indexed (too big): its ETag is
    /// remembered so it is not refetched on every search.
    pub fn put_unindexed(&mut self, name: &str, etag: &str) {
        self.notes.insert(name.to_string(), NoteEntry { etag: etag.to_string(), chunks: Vec::new(), partial: true });
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.notes.remove(name).is_some()
    }

    /// Hybrid search over the index. Returns the best chunk per note, at most
    /// `limit` notes, fused from BM25 and cosine rankings.
    pub fn search(&self, query: &str, prefix: &str, limit: usize, e: &Embedder) -> Vec<Hit> {
        // the corpus: every chunk of every note under the prefix
        let mut docs: Vec<(&str, &Chunk)> = Vec::new();
        for (name, entry) in &self.notes {
            if !name.starts_with(prefix) {
                continue;
            }
            for c in &entry.chunks {
                docs.push((name.as_str(), c));
            }
        }
        if docs.is_empty() {
            return Vec::new();
        }

        // ---- BM25 over exact terms (name segments count as text)
        let qterms: Vec<String> = terms(query);
        let tokenized: Vec<Vec<String>> = docs
            .iter()
            .map(|(name, c)| {
                let mut t = terms(&name.replace(['/', '.', '-', '_'], " "));
                t.extend(terms(&c.text));
                t
            })
            .collect();
        let n = docs.len() as f32;
        let avgdl = tokenized.iter().map(|t| t.len()).sum::<usize>() as f32 / n;
        let mut df: HashMap<&str, usize> = HashMap::new();
        for t in &tokenized {
            let uniq: HashSet<&str> = t.iter().map(String::as_str).collect();
            for u in uniq {
                *df.entry(u).or_insert(0) += 1;
            }
        }
        let mut bm25: Vec<(usize, f32)> = Vec::new();
        for (i, t) in tokenized.iter().enumerate() {
            let dl = t.len() as f32;
            let mut tf: HashMap<&str, f32> = HashMap::new();
            for w in t {
                *tf.entry(w.as_str()).or_insert(0.0) += 1.0;
            }
            let mut s = 0f32;
            for q in &qterms {
                let Some(&f) = tf.get(q.as_str()) else { continue };
                let d = *df.get(q.as_str()).unwrap_or(&0) as f32;
                let idf = (1.0 + (n - d + 0.5) / (d + 0.5)).ln();
                s += idf * (f * (BM25_K1 + 1.0)) / (f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avgdl));
            }
            if s > 0.0 {
                bm25.push((i, s));
            }
        }
        bm25.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // ---- cosine over the vectors
        let qv = e.embed(query);
        let mut cos: Vec<(usize, f32)> = docs
            .iter()
            .enumerate()
            .map(|(i, (_, c))| (i, dot_q(&qv, &c.vec, c.scale)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        cos.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // ---- reciprocal rank fusion of the two lists' heads
        let head = (limit * 5).clamp(20, 100);
        let mut fused: HashMap<usize, (f32, Option<usize>, Option<usize>)> = HashMap::new();
        for (rank, (i, _)) in bm25.iter().take(head).enumerate() {
            let f = fused.entry(*i).or_insert((0.0, None, None));
            f.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
            f.1 = Some(rank + 1);
        }
        for (rank, (i, _)) in cos.iter().take(head).enumerate() {
            let f = fused.entry(*i).or_insert((0.0, None, None));
            f.0 += 1.0 / (RRF_K + rank as f32 + 1.0);
            f.2 = Some(rank + 1);
        }
        let mut ranked: Vec<(usize, f32, Option<usize>, Option<usize>)> =
            fused.into_iter().map(|(i, (s, b, v))| (i, s, b, v)).collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));

        // best chunk per note
        let mut seen: HashSet<&str> = HashSet::new();
        let mut hits = Vec::new();
        for (i, score, b, v) in ranked {
            let (name, c) = docs[i];
            if !seen.insert(name) {
                continue;
            }
            hits.push(Hit {
                name: name.to_string(),
                line: c.line,
                text: snippet(&c.text, &qterms),
                score,
                bm25_rank: b,
                vector_rank: v,
            });
            if hits.len() >= limit {
                break;
            }
        }
        hits
    }
}

#[derive(Serialize)]
pub struct Hit {
    pub name: String,
    pub line: usize,
    pub text: String,
    pub score: f32,
    pub bm25_rank: Option<usize>,
    pub vector_rank: Option<usize>,
}

/// BM25's tokens: lowercase runs of letters and digits, two characters or
/// more, minus the commonest English function words. No stemming; the
/// vectors carry the fuzziness, this side carries exactness. The stop list
/// matters in a SMALL notebook, where "the" in one chunk of five would
/// otherwise carry real idf; in a large one idf retires it anyway.
pub fn terms(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2 && !STOP.contains(t))
        .map(str::to_string)
        .collect()
}

const STOP: &[&str] = &[
    "the", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "was", "were",
    "be", "been", "it", "its", "this", "that", "these", "those", "you", "we", "they", "he", "she",
    "do", "does", "did", "what", "how", "why", "when", "which", "who", "my", "me", "our", "your",
    "at", "by", "from", "as", "about", "into", "not", "no", "so", "if", "then", "than", "but",
];

/// Split a note into chunks of roughly CHUNK_TARGET characters on paragraph
/// boundaries (blank lines and headings), splitting an oversize paragraph at
/// whitespace. Returns (start line, text) pairs and whether the note had
/// more than MAX_CHUNKS_PER_NOTE chunks' worth.
pub fn chunk(text: &str) -> (Vec<(usize, String)>, bool) {
    // paragraphs with their starting line numbers (1-based)
    let mut paras: Vec<(usize, String)> = Vec::new();
    let mut cur = String::new();
    let mut cur_line = 1;
    for (i, line) in text.lines().enumerate() {
        let lineno = i + 1;
        let is_heading = line.trim_start().starts_with('#');
        if line.trim().is_empty() || is_heading {
            if !cur.trim().is_empty() {
                paras.push((cur_line, std::mem::take(&mut cur)));
            } else {
                cur.clear();
            }
            cur_line = lineno;
            if is_heading {
                cur.push_str(line);
                cur.push('\n');
            }
            continue;
        }
        if cur.is_empty() {
            cur_line = lineno;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        paras.push((cur_line, cur));
    }
    // split any oversize paragraph at whitespace
    let mut pieces: Vec<(usize, String)> = Vec::new();
    for (line, p) in paras {
        if p.chars().count() <= CHUNK_HARD {
            pieces.push((line, p));
            continue;
        }
        let mut buf = String::new();
        let mut buf_line = line;
        let mut seen_lines = 0usize;
        for word in p.split_inclusive(char::is_whitespace) {
            if buf.chars().count() + word.chars().count() > CHUNK_TARGET && !buf.is_empty() {
                pieces.push((buf_line, std::mem::take(&mut buf)));
                buf_line = line + seen_lines;
            }
            seen_lines += word.matches('\n').count();
            buf.push_str(word);
        }
        if !buf.trim().is_empty() {
            pieces.push((buf_line, buf));
        }
    }
    // merge small neighbours up to the target
    let mut out: Vec<(usize, String)> = Vec::new();
    for (line, p) in pieces {
        if let Some((_, last)) = out.last_mut() {
            if last.chars().count() + p.chars().count() <= CHUNK_TARGET {
                if !last.ends_with('\n') {
                    last.push('\n');
                }
                last.push_str(&p);
                continue;
            }
        }
        out.push((line, p));
    }
    let partial = out.len() > MAX_CHUNKS_PER_NOTE;
    out.truncate(MAX_CHUNKS_PER_NOTE);
    for (_, t) in out.iter_mut() {
        let trimmed = t.trim_end().to_string();
        *t = trimmed;
    }
    (out, partial)
}

/// A readable excerpt of a chunk: the window around the first query term if
/// one occurs, else the start.
fn snippet(text: &str, qterms: &[String]) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= SNIPPET_CHARS {
        return flat;
    }
    let lower = flat.to_lowercase();
    let pos = qterms.iter().filter_map(|q| lower.find(q.as_str())).min().unwrap_or(0);
    let char_pos = lower[..pos].chars().count();
    let start = char_pos.saturating_sub(SNIPPET_CHARS / 3);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

/// int8 with one scale per vector; the vectors are unit length going in, so
/// the dot product with a unit query is a cosine to within the quantisation.
pub fn quantize(v: &[f32]) -> (Vec<u8>, f32) {
    let m = v.iter().fold(0f32, |a, x| a.max(x.abs()));
    let scale = if m > 0.0 { m / 127.0 } else { 1.0 };
    (v.iter().map(|x| ((x / scale).round().clamp(-127.0, 127.0) as i8) as u8).collect(), scale)
}

fn dot_q(q: &[f32], b64: &str, scale: f32) -> f32 {
    let Ok(bytes) = B64.decode(b64) else { return 0.0 };
    if bytes.len() != q.len() {
        return 0.0;
    }
    q.iter().zip(bytes).map(|(x, b)| x * (b as i8) as f32 * scale).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_follow_paragraphs_and_headings() {
        let text = "# Title\n\nfirst para line one\nline two\n\n## Second\nsecond para\n";
        let (c, partial) = chunk(text);
        assert!(!partial);
        assert_eq!(c.len(), 1, "small paragraphs merge: {c:?}");
        assert_eq!(c[0].0, 1);
        let big = "word ".repeat(1000);
        let (c, _) = chunk(&big);
        assert!(c.len() >= 5 && c.iter().all(|(_, t)| t.chars().count() <= CHUNK_HARD));
        let many = (0..200).map(|i| format!("para {i} {}\n\n", "x".repeat(700))).collect::<String>();
        let (c, partial) = chunk(&many);
        assert_eq!(c.len(), MAX_CHUNKS_PER_NOTE);
        assert!(partial);
        assert_eq!(c[1].0, 3, "second chunk starts at its own line");
    }

    #[test]
    fn quantization_keeps_cosine() {
        let e = Embedder::new().unwrap();
        let v = e.embed("the fleet's egress is ipv6 only");
        let (q, scale) = quantize(&v);
        let back = dot_q(&v, &B64.encode(&q), scale);
        assert!((back - 1.0).abs() < 0.02, "{back}");
    }

    #[test]
    fn hybrid_finds_exact_and_semantic_matches() {
        let e = Embedder::new().unwrap();
        let mut idx = Index::empty(crate::embed::MODEL, e.dim());
        idx.put_note("infra/egress.md", "e1", "The fleet's outbound traffic leaves over IPv6 only.\nR2 publishes AAAA records, so the bucket is reachable.\n", &e);
        idx.put_note("home/groceries.md", "e2", "Grocery list: eggs, milk, bread, butter.\n", &e);
        idx.put_note("money/taxes.md", "e3", "Meeting with the accountant about the quarterly tax filing.\n", &e);
        // exact term: BM25 carries it
        let h = idx.search("AAAA records", "", 3, &e);
        assert_eq!(h[0].name, "infra/egress.md");
        assert!(h[0].bm25_rank.is_some());
        // no shared term: the vectors carry it
        let h = idx.search("network connectivity", "", 3, &e);
        assert_eq!(h[0].name, "infra/egress.md", "{:?}", h.iter().map(|x| (&x.name, x.score)).collect::<Vec<_>>());
        let h = idx.search("what do I owe the government", "", 3, &e);
        assert_eq!(h[0].name, "money/taxes.md");
        // prefix scopes, limit caps, one hit per note
        assert!(idx.search("eggs", "infra/", 3, &e).is_empty());
        assert_eq!(idx.search("eggs", "", 1, &e).len(), 1);
        assert!(terms("what do I owe the government") == ["owe", "government"], "{:?}", terms("what do I owe the government"));
        // reconcile: a changed etag is stale, a missing note is gone
        let live = vec![("infra/egress.md".to_string(), "e1".to_string(), 10), ("home/groceries.md".to_string(), "e9".to_string(), 10)];
        let (stale, gone) = idx.reconcile(&live);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].0, "home/groceries.md");
        assert_eq!(gone, vec!["money/taxes.md".to_string()]);
        // a round trip through JSON keeps everything
        let bytes = serde_json::to_vec(&idx).unwrap();
        let back = Index::parse(&bytes, crate::embed::MODEL, e.dim());
        assert_eq!(back.chunk_count(), idx.chunk_count());
        assert!(Index::parse(&bytes, "other-model", e.dim()).notes.is_empty());
    }

    #[test]
    fn terms_and_snippets() {
        assert_eq!(terms("IPv6-only egress, R2!"), ["ipv6", "only", "egress", "r2"]);
        assert_eq!(terms("the AAAA records of the bucket"), ["aaaa", "records", "bucket"]);
        let long = format!("{} needle {}", "a ".repeat(500), "b ".repeat(500));
        let s = snippet(&long, &["needle".to_string()]);
        assert!(s.contains("needle") && s.chars().count() <= SNIPPET_CHARS + 2);
    }
}
