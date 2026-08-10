//! Standard base64 (RFC 4648, with padding), host-compilable so `cargo
//! test` covers it natively. Encode feeds every image-bearing JSON response;
//! decode feeds /v1/images/upscale's `image` field.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Decode standard base64. Tolerant of whitespace (JSON-pasted blobs wrap)
/// and of absent padding; refuses other characters.
pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a') as u32 + 26),
            b'0'..=b'9' => Ok((c - b'0') as u32 + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 character {:?}", c as char)),
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut done = false; // padding seen: only trailing whitespace may follow
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            done = true;
            continue;
        }
        if done {
            return Err("base64 data after padding".into());
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // leftover bits under a byte are padding artifacts; 6 leftover bits
    // (a lone trailing character) cannot come from a valid encoding
    if bits >= 6 {
        return Err("truncated base64".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0u8, 255, 128, 7]] {
            assert_eq!(decode(&encode(data)).unwrap(), data, "{data:?}");
        }
    }

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode("Zm9v\nYmFy").unwrap(), b"foobar"); // wrapped
        assert_eq!(decode("Zm8=").unwrap(), b"fo"); // padded
        assert_eq!(decode("Zm8").unwrap(), b"fo"); // unpadded
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("Zm9v!").is_err());
        assert!(decode("Zm8=x").is_err()); // data after padding
        assert!(decode("Z").is_err()); // lone char
    }
}
