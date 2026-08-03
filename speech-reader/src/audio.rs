//! Audio admission and chunking, guest-side.
//!
//! The HOST decodes audio (libmtmd's miniaudio reads wav/mp3/flac and
//! resamples to the encoder's 16 kHz); this module only does what admission
//! control and long-form chunking need: sniff the container, read WAV headers
//! for duration, downmix stereo, and split long PCM at quiet points so each
//! piece is one transcription episode. Compressed formats pass through whole -
//! parsing mp3 frame headers for a duration estimate is a rabbit hole this
//! component stays out of; the byte cap and the model's own position budget
//! are their admission control.
//!
//! Anything miniaudio cannot read (ogg/opus/webm/m4a - notably everything a
//! browser's MediaRecorder produces) is REFUSED HERE with the reason, which is
//! why the playground records raw PCM and builds its own WAV instead of using
//! MediaRecorder at all.

/// What the bytes are, by magic. `Wav` may still turn out malformed - parse()
/// decides that - but a wrong container is named before any model work.
#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Kind {
    Wav,
    Mp3,
    Flac,
}

pub fn sniff(b: &[u8]) -> Result<Kind, String> {
    if b.len() < 12 {
        return Err("[audio_undecodable] attachment is too short to be audio".into());
    }
    if b.starts_with(b"RIFF") && &b[8..12] == b"WAVE" {
        return Ok(Kind::Wav);
    }
    if b.starts_with(b"fLaC") {
        return Ok(Kind::Flac);
    }
    if b.starts_with(b"ID3") || (b[0] == 0xff && (b[1] & 0xe0) == 0xe0) {
        return Ok(Kind::Mp3);
    }
    let named = if b.starts_with(b"OggS") {
        "ogg/opus"
    } else if b.len() > 11 && &b[4..8] == b"ftyp" {
        "mp4/m4a"
    } else if b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        "webm/matroska"
    } else {
        ""
    };
    if !named.is_empty() {
        return Err(format!(
            "[audio_undecodable] {named} is not a format this deployment reads - send wav, mp3 \
             or flac (the playground records WAV directly; `ffmpeg -i in -ar 16000 -ac 1 \
             out.wav` converts anything)"
        ));
    }
    Err("[audio_undecodable] attachment is not recognisable audio (wav, mp3 or flac)".into())
}

/// A parsed WAV: enough structure to know its length and to cut it.
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    /// PCM frames (one sample per channel), s16 interleaved - only when the
    /// encoding is 16-bit integer PCM, which is what makes cutting possible
    pub s16: Option<Vec<i16>>,
    pub seconds: f32,
}

pub fn parse_wav(b: &[u8]) -> Result<Wav, String> {
    let err = |m: &str| format!("[audio_undecodable] {m}");
    if b.len() < 44 || !b.starts_with(b"RIFF") || &b[8..12] != b"WAVE" {
        return Err(err("not a RIFF/WAVE file"));
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let sz = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_end = (pos + 8).saturating_add(sz).min(b.len());
        let body = &b[pos + 8..body_end];
        match id {
            b"fmt " if body.len() >= 16 => {
                fmt = Some((
                    u16::from_le_bytes(body[0..2].try_into().unwrap()),
                    u16::from_le_bytes(body[2..4].try_into().unwrap()),
                    u32::from_le_bytes(body[4..8].try_into().unwrap()),
                    u16::from_le_bytes(body[14..16].try_into().unwrap()),
                ));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        pos = body_end + (sz & 1); // chunks are word-aligned
    }
    let (format, channels, sample_rate, bits) = fmt.ok_or_else(|| err("no fmt chunk"))?;
    let data = data.ok_or_else(|| err("no data chunk"))?;
    if channels == 0 || sample_rate == 0 {
        return Err(err("fmt chunk declares zero channels or rate"));
    }
    let bytes_per_sample = (bits as usize).div_ceil(8);
    let frame_bytes = bytes_per_sample * channels as usize;
    if frame_bytes == 0 || data.len() < frame_bytes {
        return Err(err("data chunk holds no audio"));
    }
    let n_frames = data.len() / frame_bytes;
    let seconds = n_frames as f32 / sample_rate as f32;
    // 16-bit integer PCM (format 1, or extensible 0xFFFE with 16 bits) is the
    // cuttable case; anything else is admitted by duration and passed whole
    let s16 = if (format == 1 || format == 0xFFFE) && bits == 16 {
        Some(
            data[..n_frames * frame_bytes]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect(),
        )
    } else {
        None
    };
    Ok(Wav { sample_rate, channels, s16, seconds })
}

/// Interleaved s16 -> mono by averaging. Halves (or better) what crosses to
/// the host, and transcription models are mono creatures anyway.
pub fn downmix(samples: &[i16], channels: u16) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let c = channels as usize;
    samples
        .chunks_exact(c)
        .map(|f| (f.iter().map(|&s| s as i32).sum::<i32>() / c as i32) as i16)
        .collect()
}

/// Build a minimal mono 16-bit WAV around raw samples.
pub fn wav_bytes(mono: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (mono.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + mono.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in mono {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Cut mono PCM into runs of at most `chunk_seconds`, each cut placed at the
/// QUIETEST 100 ms in the final quarter of the allowed span - a word is never
/// split when a breath is available. Returns (samples, seconds) per chunk.
pub fn chunk_pcm(mono: &[i16], sample_rate: u32, chunk_seconds: usize) -> Vec<(Vec<i16>, f32)> {
    let sr = sample_rate as usize;
    let max = chunk_seconds.max(10) * sr;
    if mono.len() <= max {
        return vec![(mono.to_vec(), mono.len() as f32 / sr as f32)];
    }
    let win = sr / 10; // 100 ms energy window
    let mut out = Vec::new();
    let mut start = 0usize;
    while mono.len() - start > max {
        // search [start + 3/4 max, start + max) for the quietest window
        let lo = start + max * 3 / 4;
        let hi = start + max;
        let mut best = hi - win;
        let mut best_e = u64::MAX;
        let mut i = lo;
        while i + win <= hi {
            let e: u64 = mono[i..i + win].iter().map(|&s| (s as i64).unsigned_abs()).sum();
            if e < best_e {
                best_e = e;
                best = i;
            }
            i += win / 2;
        }
        let cut = best + win / 2;
        out.push((mono[start..cut].to_vec(), (cut - start) as f32 / sr as f32));
        start = cut;
    }
    out.push((mono[start..].to_vec(), (mono.len() - start) as f32 / sr as f32));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_wav(seconds: f32, rate: u32, channels: u16) -> Vec<u8> {
        let n = (seconds * rate as f32) as usize;
        let mut mono = Vec::with_capacity(n);
        for i in 0..n {
            let v = ((i as f32 * 0.05).sin() * 8000.0) as i16;
            mono.push(v);
        }
        if channels == 2 {
            let inter: Vec<i16> = mono.iter().flat_map(|&s| [s, s / 2]).collect();
            // hand-build a stereo wav
            let mut b = wav_bytes(&mono, rate);
            let data: Vec<u8> = inter.iter().flat_map(|s| s.to_le_bytes()).collect();
            let dl = data.len() as u32;
            b.truncate(44);
            b[22] = 2; // channels
            let br = rate * 4;
            b[28..32].copy_from_slice(&br.to_le_bytes());
            b[32] = 4; // block align
            b[4..8].copy_from_slice(&(36 + dl).to_le_bytes());
            b[40..44].copy_from_slice(&dl.to_le_bytes());
            b.extend_from_slice(&data);
            b
        } else {
            wav_bytes(&mono, rate)
        }
    }

    #[test]
    fn sniffing_names_what_it_refuses() {
        assert_eq!(sniff(&tone_wav(0.1, 16000, 1)).unwrap(), Kind::Wav);
        assert_eq!(sniff(b"fLaC12345678").unwrap(), Kind::Flac);
        assert_eq!(sniff(b"ID3\x04\x00\x00\x00\x00\x00\x00\x00\x00").unwrap(), Kind::Mp3);
        let mp3_frame = [0xffu8, 0xfb, 0x90, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(sniff(&mp3_frame).unwrap(), Kind::Mp3);
        for (bytes, name) in [
            (b"OggS\0\0\0\0\0\0\0\0".to_vec(), "ogg"),
            (b"\x00\x00\x00\x20ftypisom\0\0\0\0".to_vec(), "mp4"),
            (vec![0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0, 0, 0, 0, 0], "webm"),
        ] {
            let e = sniff(&bytes).err().unwrap();
            assert!(e.contains(name), "{name}: {e}");
            assert!(e.contains("[audio_undecodable]"));
        }
    }

    #[test]
    fn wav_parse_reads_the_geometry_and_round_trips() {
        let w = parse_wav(&tone_wav(2.0, 16000, 1)).unwrap();
        assert_eq!(w.sample_rate, 16000);
        assert_eq!(w.channels, 1);
        assert!((w.seconds - 2.0).abs() < 0.01);
        let s16 = w.s16.unwrap();
        assert_eq!(s16.len(), 32000);
        // rebuild and re-parse
        let again = parse_wav(&wav_bytes(&s16, 16000)).unwrap();
        assert_eq!(again.s16.unwrap(), s16);
    }

    #[test]
    fn stereo_downmixes_to_the_average() {
        let w = parse_wav(&tone_wav(1.0, 16000, 2)).unwrap();
        assert_eq!(w.channels, 2);
        let s = w.s16.unwrap();
        let mono = downmix(&s, 2);
        assert_eq!(mono.len(), s.len() / 2);
        assert_eq!(mono[10], ((s[20] as i32 + s[21] as i32) / 2) as i16);
    }

    #[test]
    fn long_audio_cuts_at_the_quiet_spot() {
        // 30 s of tone with a silent gap at 21.5 s; chunk at 25 s max
        let sr = 8000usize;
        let mut mono: Vec<i16> = (0..30 * sr).map(|i| ((i as f32 * 0.3).sin() * 9000.0) as i16).collect();
        let gap = (21.5 * sr as f32) as usize;
        for v in &mut mono[gap..gap + sr / 4] {
            *v = 0;
        }
        let chunks = chunk_pcm(&mono, sr as u32, 25);
        assert_eq!(chunks.len(), 2);
        // the cut landed inside the silence, not at the 25 s wall
        let cut_at = chunks[0].0.len() as f32 / sr as f32;
        assert!((21.4..21.8).contains(&cut_at), "cut at {cut_at}");
        // nothing lost
        assert_eq!(chunks[0].0.len() + chunks[1].0.len(), mono.len());
    }

    #[test]
    fn short_audio_is_one_chunk_and_malformed_wav_is_named() {
        let mono: Vec<i16> = vec![0; 8000];
        assert_eq!(chunk_pcm(&mono, 8000, 240).len(), 1);
        assert!(parse_wav(b"RIFFxxxxWAVE").err().unwrap().contains("[audio_undecodable]"));
        let mut no_data = tone_wav(0.5, 8000, 1);
        no_data.truncate(40); // cut inside the header
        assert!(parse_wav(&no_data).is_err());
    }
}
