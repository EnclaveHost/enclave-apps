//! WAV framing for a stream whose length is unknown when the header goes out.
//!
//! The header is written ONCE, before generation starts (it doubles as the
//! keepalive that proves the response is alive), so the RIFF and data sizes
//! cannot be real. The streaming convention - 0xFFFFFFFF in both size fields -
//! is what every serious reader (ffmpeg, browsers, sox, players) treats as
//! "read until the transport ends". 16-bit PCM mono at the SNAC rate.

use crate::snac::SAMPLE_RATE;

pub const STREAMING_SIZE: u32 = 0xFFFF_FFFF;

/// The 44-byte canonical header. `data_bytes` = STREAMING_SIZE for a chunked
/// response; an exact figure when the caller buffered the whole take.
pub fn header(data_bytes: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let riff_size = if data_bytes == STREAMING_SIZE {
        STREAMING_SIZE
    } else {
        36 + data_bytes
    };
    let byte_rate = SAMPLE_RATE * 2; // mono, 16-bit
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff_size.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    h[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}

/// f32 [-1, 1] -> little-endian i16 bytes. Symmetric 32767 scaling with a
/// clamp: the decoder ends in tanh so overs are rare, but one clipped sample
/// must saturate rather than wrap.
pub fn pcm16_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_a_valid_riff_preamble() {
        let h = header(1000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 1036);
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 24000);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 1000);
    }

    #[test]
    fn the_streaming_header_says_read_until_eof() {
        let h = header(STREAMING_SIZE);
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), STREAMING_SIZE);
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), STREAMING_SIZE);
    }

    #[test]
    fn pcm_conversion_saturates_and_round_trips_scale() {
        let b = pcm16_bytes(&[0.0, 1.0, -1.0, 2.0, -2.0, 0.5]);
        let v: Vec<i16> = b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(v[0], 0);
        assert_eq!(v[1], 32767);
        assert_eq!(v[2], -32767);
        assert_eq!(v[3], 32767, "over must clamp, not wrap");
        assert_eq!(v[4], -32767);
        assert_eq!(v[5], 16384);
    }
}
