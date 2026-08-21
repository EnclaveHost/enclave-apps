//! leavescache — the index's (key, size, etag, leaves) rows, persisted to the
//! bucket so a restart reuses them instead of re-hashing every object.
//!
//! Why: the index only serves what it has hashed, so a restart answered 404
//! for EVERY CID until a full-bucket re-hash finished (~80 minutes at 3.9 GB,
//! observed live 2026-08-21) — and a fleet repoint restarts this app together
//! with the enclaves that prefetch from it, which turned each repoint into a
//! platform-wide "prefetch failed" window. With the rows persisted, a boot is
//! one LIST plus one small GET, and only objects that actually changed hash.
//!
//! Trust: identical to the in-memory reuse across refreshes — a row is only
//! believed for an object whose (size, etag) still match the fresh listing,
//! the same binding start_hash() already trusts across refreshes. Whoever can
//! forge this object in the bucket can already rewrite the pinned bytes it
//! describes, and consumers verify fetched bytes against the CID regardless.
//!
//! Format (little-endian, self-delimiting, rejects anything out of bounds):
//!   magic "s3ipfs-leaves:1\n"
//!   u32 row count
//!   per row: u16 key len, key bytes, u64 size, u16 etag len, etag bytes,
//!            u32 leaf count, leaf count * 32 digest bytes

use crate::ipfs::CHUNK;

const MAGIC: &[u8] = b"s3ipfs-leaves:1\n";
/// S3 keys are capped at 1024 bytes; anything longer is corruption.
const MAX_KEY: usize = 1024;
/// ETags observed are hex or hex-N multipart markers; 256 is generous.
const MAX_ETAG: usize = 256;
/// 4M leaves = a 1 TiB object; far beyond MAX_UPLOAD, so: corruption.
const MAX_LEAVES: u32 = 1 << 22;
/// A million rows is far beyond maxKeys; anything claiming more is corruption.
const MAX_ROWS: u32 = 1 << 20;

pub struct Row {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub leaves: Vec<[u8; 32]>,
}

pub fn serialize<'a, I>(rows: I) -> Vec<u8>
where
    I: Iterator<Item = (&'a str, u64, &'a str, &'a [[u8; 32]])> + Clone,
{
    let n = rows.clone().count() as u32;
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&n.to_le_bytes());
    for (key, size, etag, leaves) in rows {
        out.extend_from_slice(&(key.len() as u16).to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(etag.len() as u16).to_le_bytes());
        out.extend_from_slice(etag.as_bytes());
        out.extend_from_slice(&(leaves.len() as u32).to_le_bytes());
        for l in leaves {
            out.extend_from_slice(l);
        }
    }
    out
}

/// Any structural defect rejects the WHOLE cache: a partial read of a
/// corrupted object must degrade to "no cache" (full re-hash, today's
/// behavior), never to serving half a world.
pub fn deserialize(data: &[u8]) -> Result<Vec<Row>, String> {
    let mut p = data;
    let mut take = |n: usize, what: &str| -> Result<&[u8], String> {
        if p.len() < n {
            return Err(format!("truncated at {what}"));
        }
        let (a, b) = p.split_at(n);
        p = b;
        Ok(a)
    };
    if take(MAGIC.len(), "magic")? != MAGIC {
        return Err("bad magic".into());
    }
    let n = u32::from_le_bytes(take(4, "count")?.try_into().unwrap());
    if n > MAX_ROWS {
        return Err(format!("row count {n} out of bounds"));
    }
    let mut rows = Vec::new();
    for i in 0..n {
        let klen = u16::from_le_bytes(take(2, "key len")?.try_into().unwrap()) as usize;
        if klen == 0 || klen > MAX_KEY {
            return Err(format!("row {i}: key length {klen} out of bounds"));
        }
        let key = std::str::from_utf8(take(klen, "key")?)
            .map_err(|_| format!("row {i}: key is not utf8"))?
            .to_string();
        let size = u64::from_le_bytes(take(8, "size")?.try_into().unwrap());
        let elen = u16::from_le_bytes(take(2, "etag len")?.try_into().unwrap()) as usize;
        if elen > MAX_ETAG {
            return Err(format!("row {i}: etag length {elen} out of bounds"));
        }
        let etag = std::str::from_utf8(take(elen, "etag")?)
            .map_err(|_| format!("row {i}: etag is not utf8"))?
            .to_string();
        let nl = u32::from_le_bytes(take(4, "leaf count")?.try_into().unwrap());
        if nl > MAX_LEAVES {
            return Err(format!("row {i}: leaf count {nl} out of bounds"));
        }
        // The leaf count is DERIVED state: it must match the size exactly
        // (empty objects carry the one empty-chunk digest), or range serving
        // would address chunks that do not exist.
        let want = if size == 0 { 1 } else { size.div_ceil(CHUNK) };
        if u64::from(nl) != want {
            return Err(format!("row {i}: {nl} leaves for {size} bytes (want {want})"));
        }
        let mut leaves = Vec::with_capacity(nl as usize);
        for _ in 0..nl {
            leaves.push(<[u8; 32]>::try_from(take(32, "leaf")?).unwrap());
        }
        rows.push(Row { key, size, etag, leaves });
    }
    if !p.is_empty() {
        return Err(format!("{} trailing bytes", p.len()));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Row> {
        vec![
            Row { key: "pins/bafk1".into(), size: 5, etag: "\"abc\"".into(), leaves: vec![[1; 32]] },
            Row { key: "a/b c/d".into(), size: CHUNK * 2 + 1, etag: "\"e-2\"".into(), leaves: vec![[2; 32], [3; 32], [4; 32]] },
            Row { key: "empty".into(), size: 0, etag: String::new(), leaves: vec![[5; 32]] },
        ]
    }

    fn ser(rows: &[Row]) -> Vec<u8> {
        serialize(rows.iter().map(|r| (r.key.as_str(), r.size, r.etag.as_str(), r.leaves.as_slice())))
    }

    #[test]
    fn roundtrip() {
        let rows = sample();
        let got = deserialize(&ser(&rows)).unwrap();
        assert_eq!(got.len(), rows.len());
        for (g, w) in got.iter().zip(&rows) {
            assert_eq!((&g.key, g.size, &g.etag, &g.leaves), (&w.key, w.size, &w.etag, &w.leaves));
        }
    }

    #[test]
    fn empty_cache_roundtrips() {
        assert!(deserialize(&ser(&[])).unwrap().is_empty());
    }

    #[test]
    fn truncation_rejects() {
        let bytes = ser(&sample());
        for cut in [1, MAGIC.len() + 2, bytes.len() - 1] {
            assert!(deserialize(&bytes[..cut]).is_err(), "cut at {cut} accepted");
        }
    }

    #[test]
    fn trailing_bytes_reject() {
        let mut bytes = ser(&sample());
        bytes.push(0);
        assert!(deserialize(&bytes).is_err());
    }

    #[test]
    fn bad_magic_rejects() {
        let mut bytes = ser(&sample());
        bytes[0] ^= 1;
        assert!(deserialize(&bytes).is_err());
    }

    #[test]
    fn leaf_count_must_match_size() {
        let rows = vec![Row { key: "k".into(), size: CHUNK + 1, etag: String::new(), leaves: vec![[0; 32]] }];
        assert!(deserialize(&ser(&rows)).is_err());
    }

    #[test]
    fn absurd_row_count_rejects() {
        let mut bytes = Vec::from(MAGIC);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(deserialize(&bytes).is_err());
    }
}
