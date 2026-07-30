//! WebP in, JPEG out.
//!
//! WebP is the one format a browser will happily hand you that the vision
//! encoder cannot read: mtmd decodes through stb_image, which does JPEG, PNG,
//! BMP and GIF and has no VP8 at all. The host's own refusal even lists webp as
//! readable, which is exactly how this went unnoticed - a webp reaches the
//! encoder, dies there, and the sentence that comes back says the format is
//! supported.
//!
//! So the picture is decoded here instead and re-encoded as JPEG before it ever
//! reaches wasi-nn. That is what the playground's canvas already does client
//! side; this does it for every OTHER caller too - an SDK posting
//! `data:image/webp`, a pasted data URI, the vision-service leg forwarding to a
//! sibling deployment.
//!
//! Big pictures are box-averaged down on the way through, to the same area cap
//! the playground uses. The encoder rescales to its own grid regardless, and it
//! keeps a 12 Mpx phone photo from becoming a multi-megabyte JPEG that the
//! vision-service leg would then refuse for being too large.

use image_webp::WebPDecoder;
use jpeg_encoder::{ColorType, Encoder};

/// Area the picture is boxed down to, matching chat.html's MAX_PIXELS so the
/// browser path and this path hand the model comparable pixels.
const MAX_PIXELS: usize = 1_150_000;

/// And an edge cap, because area alone lets a panorama through: JPEG dimensions
/// are u16, and no vision encoder wants a 30000-pixel side.
const MAX_EDGE: usize = 4096;

/// Refused before a single byte is allocated. A 12-byte header can claim
/// 16383x16383, which is a gigabyte of RGBA; nothing legitimate arrives through
/// a chat box that big.
const BOMB_PIXELS: u64 = 40_000_000;

/// Re-encode quality. 82 is the same trade the playground makes: small enough
/// that a photo fits the byte caps, high enough that JPEG ringing does not eat
/// the small text an OCR-ish question is about.
const QUALITY: u8 = 82;

/// Decode a webp and return JPEG bytes. Animated files decode to their first
/// frame, which is the frame a question about a still is about anyway.
pub fn to_jpeg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = WebPDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("that webp could not be read ({e})"))?;
    let (dw, dh) = dec.dimensions();
    let (w, h) = (dw as usize, dh as usize);
    if w == 0 || h == 0 {
        return Err("that webp declares a zero width or height".into());
    }
    if u64::from(dw) * u64::from(dh) > BOMB_PIXELS {
        return Err(format!(
            "that webp is {w}x{h}; this app will not spend the memory to decode it - \
             resize it before sending"
        ));
    }
    dec.set_memory_limit(BOMB_PIXELS as usize * 4);
    let size = dec
        .output_buffer_size()
        .ok_or("that webp is too large to decode")?;
    let mut px = vec![0u8; size];
    dec.read_image(&mut px)
        .map_err(|e| format!("that webp could not be decoded ({e})"))?;
    // The model sees RGB, so unpainted alpha would read as black. White is what
    // the playground's canvas composites onto.
    let rgb = if dec.has_alpha() {
        flatten_onto_white(&px)
    } else {
        px
    };
    let f = factor(w, h);
    let (rgb, w, h) = if f == 1 {
        (rgb, w, h)
    } else {
        box_down(&rgb, w, h, f)
    };
    let mut out = Vec::new();
    Encoder::new(&mut out, QUALITY)
        .encode(&rgb, w as u16, h as u16, ColorType::Rgb)
        .map_err(|e| format!("that webp decoded but would not re-encode as jpeg ({e})"))?;
    Ok(out)
}

/// RGBA over white, dropping alpha.
fn flatten_onto_white(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for p in rgba.chunks_exact(4) {
        let a = u32::from(p[3]);
        for &c in &p[..3] {
            out.push(((u32::from(c) * a + 255 * (255 - a)) / 255) as u8);
        }
    }
    out
}

/// The integer factor that brings w*h under the area cap AND both edges under
/// the edge cap. Integer rather than arbitrary scale so the resample is a plain
/// box average with no interpolation to get wrong.
fn factor(w: usize, h: usize) -> usize {
    let mut f = 1;
    while (w / f).max(1) * (h / f).max(1) > MAX_PIXELS || w.max(h) / f > MAX_EDGE {
        f += 1;
    }
    f
}

/// Average f-by-f blocks of RGB down to one pixel each. Averaging rather than
/// nearest-neighbour because dropping pixels is what turns fine text into
/// aliased noise, and text is what these pictures usually carry.
fn box_down(src: &[u8], w: usize, h: usize, f: usize) -> (Vec<u8>, usize, usize) {
    let (dw, dh) = ((w / f).max(1), (h / f).max(1));
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        for x in 0..dw {
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for sy in (y * f)..((y + 1) * f).min(h) {
                for sx in (x * f)..((x + 1) * f).min(w) {
                    let i = (sy * w + sx) * 3;
                    acc[0] += u32::from(src[i]);
                    acc[1] += u32::from(src[i + 1]);
                    acc[2] += u32::from(src[i + 2]);
                    n += 1;
                }
            }
            let o = (y * dw + x) * 3;
            let n = n.max(1);
            for c in 0..3 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    (out, dw, dh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image_webp::{ColorType as WebpColor, WebPEncoder};

    /// A real webp, built by the same crate that reads it back. Lossless, so a
    /// test can reason about exact pixels going in.
    fn webp(w: u32, h: u32, comps: usize, fill: &[u8]) -> Vec<u8> {
        let mut px = Vec::new();
        for _ in 0..(w as usize * h as usize) {
            px.extend_from_slice(&fill[..comps]);
        }
        let mut out = Vec::new();
        WebPEncoder::new(&mut out)
            .encode(
                &px,
                w,
                h,
                if comps == 4 {
                    WebpColor::Rgba8
                } else {
                    WebpColor::Rgb8
                },
            )
            .unwrap();
        out
    }

    /// JPEG dimensions, straight off the SOF0/SOF2 marker, so a test can check
    /// the geometry survived without pulling in a decoder.
    fn jpeg_dims(b: &[u8]) -> (usize, usize) {
        assert_eq!(&b[..2], &[0xff, 0xd8], "not a JPEG");
        let mut i = 2;
        while i + 9 < b.len() {
            assert_eq!(b[i], 0xff, "lost marker alignment at {i}");
            let m = b[i + 1];
            let len = ((b[i + 2] as usize) << 8) | b[i + 3] as usize;
            if (0xc0..=0xcf).contains(&m) && m != 0xc4 && m != 0xc8 && m != 0xcc {
                let h = ((b[i + 5] as usize) << 8) | b[i + 6] as usize;
                let w = ((b[i + 7] as usize) << 8) | b[i + 8] as usize;
                return (w, h);
            }
            i += 2 + len;
        }
        panic!("no SOF marker");
    }

    #[test]
    fn a_webp_comes_back_as_a_jpeg_of_the_same_geometry() {
        let out = to_jpeg(&webp(40, 30, 3, &[10, 200, 90])).unwrap();
        assert_eq!(&out[..3], &[0xff, 0xd8, 0xff], "JPEG SOI");
        assert_eq!(jpeg_dims(&out), (40, 30));
        // and the whole point: the encoder can read this and could not read the
        // input, so the bytes must NOT still be a RIFF container
        assert_ne!(&out[..4], b"RIFF");
    }

    #[test]
    fn transparency_flattens_onto_white_rather_than_black() {
        // a fully transparent pixel is white, not the black an unpainted RGB
        // buffer would show the model
        assert_eq!(flatten_onto_white(&[0, 0, 0, 0]), vec![255, 255, 255]);
        assert_eq!(flatten_onto_white(&[10, 20, 30, 255]), vec![10, 20, 30]);
        // half alpha sits between the colour and white
        let half = flatten_onto_white(&[0, 0, 0, 128]);
        assert!(half[0] > 120 && half[0] < 135, "got {half:?}");
        // an RGBA webp survives the round trip
        let out = to_jpeg(&webp(16, 16, 4, &[255, 0, 0, 0])).unwrap();
        assert_eq!(jpeg_dims(&out), (16, 16));
    }

    #[test]
    fn an_oversized_picture_is_boxed_down_before_encoding() {
        // 4000x3000 is 12 Mpx: over the area cap, so it shrinks
        let f = factor(4000, 3000);
        assert!(f > 1, "12 Mpx must shrink");
        assert!((4000 / f) * (3000 / f) <= MAX_PIXELS, "still over the cap at f={f}");
        // a panorama is under the area cap but over the edge cap, and JPEG
        // dimensions are u16, so the edge rule has to bite
        let g = factor(30000, 30);
        assert!(30000 / g <= MAX_EDGE, "edge cap ignored at f={g}");
        // small pictures are handed through untouched
        assert_eq!(factor(800, 600), 1);
        // and the average is an average: a 2x2 block of one colour stays that
        // colour, and mixed values land in between
        let src = vec![0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255];
        let (out, w, h) = box_down(&src, 2, 2, 2);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out, vec![191, 191, 191]);
    }

    #[test]
    fn junk_that_only_looks_like_a_webp_fails_with_a_sentence() {
        let e = to_jpeg(b"RIFF\0\0\0\0WEBPVP8 nope").unwrap_err();
        assert!(e.contains("webp"), "{e}");
        // no panic, no unwrap: a bad upload is a 400, not a trap
        assert!(to_jpeg(&[]).is_err());
        assert!(to_jpeg(b"RIFF").is_err());
    }
}
