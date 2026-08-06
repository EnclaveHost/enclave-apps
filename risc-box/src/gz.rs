//! Just enough gzip to read a compressed guest image.
//!
//! Guest disks are mostly empty: a 320 MiB ext2 carrying ~143 MiB of files
//! gzips to about 53 MiB, a 6.3x reduction. That matters more here than it
//! looks, because fetching the image blocks the event loop — the machine, the
//! console and every other client wait on it — so six times less to download
//! is six times less of the boot spent stalled.
//!
//! Only the reading half exists, and only for what a bucket actually serves:
//! DEFLATE (the sole method gzip has ever defined), optional header fields,
//! no multi-member streams. `miniz_oxide` does the inflating; this is the
//! container around it.
//!
//! Decompressing into a preallocated buffer is the whole point of doing this
//! by hand rather than calling `decompress_to_vec_with_limit`: that grows its
//! output by doubling, so a 320 MiB image would transiently hold 640 MiB, and
//! peak memory is exactly what this change exists to reduce. gzip stores the
//! uncompressed length in its trailer, so the right size is known up front.

/// The largest image this will inflate. Well past any sane guest disk, and far
/// past what the app's wasm32 address space could hold beside the running
/// machine anyway; it exists so a corrupt or hostile trailer asks for a
/// refusal rather than a 4 GiB allocation.
const MAX_IMAGE: usize = 2 * 1024 * 1024 * 1024;

/// Does this key name a gzip-compressed object? Suffix rather than sniffing
/// the bytes: the config author says what the bucket holds, so a mislabelled
/// object is a loud error instead of a silent guess.
pub fn is_gzip_key(key: &str) -> bool {
    key.ends_with(".gz") || key.ends_with(".gzip")
}

/// CRC-32 (the IEEE polynomial, reflected), computed on the fly so the table
/// costs nothing in the binary. gzip's trailer carries it, and a reader that
/// checks it — including every `gunzip` on the far side of a bucket — will
/// reject a member that lacks a correct one.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Wrap bytes as a gzip member.
///
/// This exists so saving is the inverse of booting. `saveKey` falls back to the
/// `fs` key, so a machine booted from `rootfs.ext2.gz` would otherwise have its
/// disk written back as raw bytes under a `.gz` name — an object that boots
/// exactly once more, then fails forever with "bad magic". Writing real gzip
/// keeps the round trip closed, and uploads about six times less.
pub fn gzip(src: &[u8]) -> Vec<u8> {
    // FLG=0 (no optional fields), MTIME=0 so the output depends only on the
    // input, XFL=0, OS=255 ("unknown") rather than claiming a filesystem.
    let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(src, 6));
    out.extend_from_slice(&crc32(src).to_le_bytes());
    out.extend_from_slice(&(src.len() as u32).to_le_bytes());
    out
}

/// Inflate a gzip member, returning the original bytes.
pub fn gunzip(src: &[u8]) -> Result<Vec<u8>, String> {
    if src.len() < 18 {
        return Err(format!("gzip: too short ({} bytes)", src.len()));
    }
    if src[0] != 0x1f || src[1] != 0x8b {
        return Err("gzip: bad magic (is the object actually gzipped?)".into());
    }
    if src[2] != 8 {
        return Err(format!("gzip: unsupported compression method {}", src[2]));
    }

    // FLG bits: 1 FHCRC, 2 FEXTRA, 3 FNAME, 4 FCOMMENT. Each optional field
    // that is present has to be stepped over to find the deflate stream.
    let flg = src[3];
    let mut p = 10usize;
    let need = |p: usize, n: usize| -> Result<(), String> {
        match p + n <= src.len() {
            true => Ok(()),
            false => Err("gzip: header runs past end of data".into()),
        }
    };
    if flg & 0b0000_0100 != 0 {
        need(p, 2)?;
        let xlen = u16::from_le_bytes([src[p], src[p + 1]]) as usize;
        p += 2 + xlen;
    }
    for bit in [0b0000_1000u8, 0b0001_0000] {
        if flg & bit != 0 {
            // NUL-terminated string
            let start = p;
            while p < src.len() && src[p] != 0 {
                p += 1;
            }
            if p >= src.len() {
                return Err("gzip: unterminated header string".into());
            }
            p += 1;
            let _ = start;
        }
    }
    if flg & 0b0000_0010 != 0 {
        p += 2; // header CRC16
    }
    if p >= src.len() - 8 {
        return Err("gzip: no deflate data".into());
    }

    // The trailer's ISIZE is the uncompressed length mod 2^32. Guest images
    // are far below 4 GiB, so it is the exact size, which is what makes a
    // single right-sized allocation possible.
    let t = src.len() - 8;
    let isize_field =
        u32::from_le_bytes([src[t + 4], src[t + 5], src[t + 6], src[t + 7]]) as usize;
    if isize_field > MAX_IMAGE {
        return Err(format!(
            "gzip: refusing to inflate {isize_field} bytes (limit {MAX_IMAGE})"
        ));
    }

    let deflate = &src[p..t];
    let mut out = vec![0u8; isize_field];
    let written = miniz_oxide::inflate::decompress_slice_iter_to_slice(
        &mut out,
        core::iter::once(deflate),
        false, // raw deflate: gzip's own header was consumed above
        true,  // adler32 is a zlib thing; gzip carries a CRC32 instead
    )
    .map_err(|e| format!("gzip: inflate failed ({e:?})"))?;

    // A short read means the stream was truncated in transit. Catching it here
    // turns a silently corrupt disk -- which would surface much later as a
    // baffling filesystem error inside the guest -- into a failed fetch that
    // the boot retry can act on.
    if written != isize_field {
        return Err(format!(
            "gzip: inflated {written} bytes, trailer promised {isize_field} (truncated download?)"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blob shaped like a real disk image: long runs of zeros with sparse
    /// content, which is what makes these images compress 6x in the first place.
    fn diskish(n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for i in (0..n).step_by(997) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v[i] = x as u8;
        }
        v
    }

    #[test]
    fn round_trip() {
        for n in [0usize, 1, 4096, 1 << 20] {
            let src = diskish(n);
            let back = gunzip(&gzip(&src)).expect("inflate own output");
            assert_eq!(back, src, "round trip failed at {n} bytes");
        }
    }

    #[test]
    fn rejects_non_gzip() {
        assert!(gunzip(&[0u8; 64]).is_err());
        assert!(gunzip(b"not even close").is_err());
    }

    #[test]
    fn rejects_truncated() {
        let full = gzip(&diskish(1 << 20));
        // Chop the deflate stream but keep a well-formed trailer, so the only
        // thing that can catch it is the inflated-length check.
        let mut cut = full[..full.len() / 2].to_vec();
        cut.extend_from_slice(&full[full.len() - 8..]);
        assert!(gunzip(&cut).is_err(), "truncated member must not inflate silently");
    }

    #[test]
    fn key_detection() {
        assert!(is_gzip_key("xfce/rootfs.ext2.gz"));
        assert!(!is_gzip_key("xfce/rootfs.ext2"));
    }
}
