//! Pure-RGB helpers, host-compilable so `cargo test` covers them natively.

/// Exact box downscale of an RGB buffer by an integer factor: each output
/// pixel is the rounded average of a `box_size` x `box_size` input block.
/// This is the supersampling half of the sub-native upscale path (e.g. a 2x
/// result = the 4x ESRGAN output averaged 2:1): averaging cancels the
/// upscaler's hallucination noise instead of resampling it, and the integer
/// geometry (dims are exact multiples, enforced by the caller's divisibility
/// check) makes it deterministic with no filter kernel to choose.
pub fn box_downscale(rgb: &[u8], width: u32, height: u32, box_size: u32) -> (Vec<u8>, u32, u32) {
    debug_assert_eq!(rgb.len(), (width * height * 3) as usize);
    debug_assert!(box_size > 0 && width % box_size == 0 && height % box_size == 0);
    if box_size <= 1 {
        return (rgb.to_vec(), width, height);
    }
    let (ow, oh) = (width / box_size, height / box_size);
    let area = (box_size * box_size) as u32;
    let half = area / 2; // rounding term
    let mut out = vec![0u8; (ow * oh * 3) as usize];
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0u32; 3];
            for dy in 0..box_size {
                let row = ((oy * box_size + dy) * width + ox * box_size) as usize * 3;
                for dx in 0..box_size {
                    let p = row + dx as usize * 3;
                    acc[0] += rgb[p] as u32;
                    acc[1] += rgb[p + 1] as u32;
                    acc[2] += rgb[p + 2] as u32;
                }
            }
            let o = (oy * ow + ox) as usize * 3;
            out[o] = ((acc[0] + half) / area) as u8;
            out[o + 1] = ((acc[1] + half) / area) as u8;
            out[o + 2] = ((acc[2] + half) / area) as u8;
        }
    }
    (out, ow, oh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_2x2_blocks_exactly() {
        // 4x2 image, two 2x2 blocks with known averages
        #[rustfmt::skip]
        let rgb = [
            10, 0, 0,  20, 0, 0,   0, 100, 0,  0, 104, 0,
            30, 0, 0,  40, 0, 0,   0, 108, 0,  0, 112, 0,
        ];
        let (out, w, h) = box_downscale(&rgb, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, vec![25, 0, 0, 0, 106, 0]);
    }

    #[test]
    fn rounds_half_up() {
        // average 1.5 rounds to 2
        let rgb = [1, 1, 1, 2, 2, 2, 1, 1, 1, 2, 2, 2];
        let (out, w, h) = box_downscale(&rgb, 2, 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![2, 2, 2]);
    }

    #[test]
    fn box_one_is_identity() {
        let rgb = [9, 8, 7, 6, 5, 4];
        let (out, w, h) = box_downscale(&rgb, 2, 1, 1);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out, rgb.to_vec());
    }

    #[test]
    fn box_four_full_collapse() {
        // 4x4 all-value-40 image collapses to one pixel of 40
        let rgb = vec![40u8; 4 * 4 * 3];
        let (out, w, h) = box_downscale(&rgb, 4, 4, 4);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![40, 40, 40]);
    }
}
