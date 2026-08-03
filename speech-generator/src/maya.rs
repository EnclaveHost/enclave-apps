//! The Maya1 token protocol: how a voice description and text become prompt
//! ids, and how the sampled audio tokens unpack into SNAC codebook streams.
//!
//! Maya1 (maya-research/maya1, Apache-2.0) is a Llama-3.2-3B whose vocabulary
//! is extended with 7 x 4096 SNAC audio tokens. A generation is a run of
//! 7-token FRAMES, each frame one 85.3 ms step of the SNAC 24 kHz codec's
//! three-level hierarchy: token 0 is the coarse (~12 Hz) code, tokens 1 and 4
//! the medium (~23 Hz) codes, tokens 2, 3, 5, 6 the fine (~47 Hz) codes. Each
//! of the 7 slots has its own 4096-id sub-range, which is what makes
//! slot-constrained sampling (sampling.rs) possible.
//!
//! Prompt shape (model card):
//!   [SOH] [BOS] <description="..."> text [TEXT_EOT] [EOH] [SOA] [SOS]
//! and the model answers with audio tokens until [CODE_EOS].

use crate::snac::Codes;

pub const BOS: u32 = 128000;
pub const TEXT_EOT: u32 = 128009;
/// start-of-speech: the last prompt token, after which audio tokens begin
pub const CODE_SOS: u32 = 128257;
/// the audio stream's own EOS
pub const CODE_EOS: u32 = 128258;
pub const SOH: u32 = 128259;
pub const EOH: u32 = 128260;
pub const SOA: u32 = 128261;
/// first audio-token id; slot j of a frame lives in
/// [SNAC_MIN + j*4096, SNAC_MIN + (j+1)*4096)
pub const SNAC_MIN: u32 = 128266;
pub const CODEBOOK: u32 = 4096;
pub const FRAME_TOKENS: usize = 7;

/// Prompt ids for one generation episode. `text_ids` is the tokenizer's
/// encoding of `<description="{desc}"> {text}` WITHOUT special tokens - the
/// specials are placed here, by id, exactly as the reference implementation
/// places them (a tokenizer asked to add its own would double the BOS).
pub fn prompt_ids(text_ids: &[u32]) -> Vec<u32> {
    let mut ids = Vec::with_capacity(text_ids.len() + 6);
    ids.push(SOH);
    ids.push(BOS);
    ids.extend_from_slice(text_ids);
    ids.push(TEXT_EOT);
    ids.push(EOH);
    ids.push(SOA);
    ids.push(CODE_SOS);
    ids
}

/// The string the tokenizer sees. The description rides inside a quoted XML
/// attribute, so quotes in it would end the attribute early - they become
/// apostrophes rather than an escape scheme the model never saw in training.
/// The text keeps its inline emotion tags (`<laugh>`, `<sigh>`, ...) verbatim:
/// they are ordinary text to the tokenizer and cues to the model.
pub fn prompt_text(desc: &str, text: &str) -> String {
    let desc = desc.replace('"', "'");
    format!("<description=\"{desc}\"> {text}")
}

/// Unpack completed frames (a multiple of 7 audio-token ids, already
/// SNAC-range-checked by the sampler) into the three SNAC codebook streams.
pub fn unpack_frames(tokens: &[u32]) -> Result<Codes, String> {
    if tokens.is_empty() || tokens.len() % FRAME_TOKENS != 0 {
        return Err(format!(
            "audio stream is {} tokens - not a whole number of 7-token frames",
            tokens.len()
        ));
    }
    let n = tokens.len() / FRAME_TOKENS;
    let mut codes = Codes {
        l1: Vec::with_capacity(n),
        l2: Vec::with_capacity(2 * n),
        l3: Vec::with_capacity(4 * n),
    };
    for f in tokens.chunks_exact(FRAME_TOKENS) {
        let mut c = [0u16; FRAME_TOKENS];
        for (j, &t) in f.iter().enumerate() {
            let lo = SNAC_MIN + j as u32 * CODEBOOK;
            if t < lo || t >= lo + CODEBOOK {
                return Err(format!(
                    "token {t} is outside slot {j}'s range [{lo}, {}) - the sampler let a \
                     non-audio token through",
                    lo + CODEBOOK
                ));
            }
            c[j] = (t - lo) as u16;
        }
        // frame layout -> hierarchy: 0 coarse; 1,4 medium; 2,3,5,6 fine
        codes.l1.push(c[0]);
        codes.l2.push(c[1]);
        codes.l2.push(c[4]);
        codes.l3.push(c[2]);
        codes.l3.push(c[3]);
        codes.l3.push(c[5]);
        codes.l3.push(c[6]);
    }
    Ok(codes)
}

/// Normalise request text: unify newlines, strip control characters the
/// tokenizer would render as noise, collapse runs of blank lines. Emotion tags
/// and ordinary punctuation pass through untouched.
pub fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let line: String =
            line.chars().filter(|c| !c.is_control() || *c == '\t').collect();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out.trim().to_string()
}

/// Split text into speaking chunks of at most `max_chars`, preferring sentence
/// boundaries, then clause boundaries, then word boundaries. Each chunk is one
/// generation episode with a fresh context - the standard long-form recipe for
/// SNAC-token models, whose single-episode budget (~25 s of audio) is a
/// training-data property rather than a config knob.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(40);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for sentence in split_sentences(text) {
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + 1 + sentence.len() > max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        if sentence.len() <= max_chars {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(sentence);
            continue;
        }
        // one sentence longer than a chunk: cut at word boundaries
        for word in sentence.split_whitespace() {
            if !current.is_empty() && current.len() + 1 + word.len() > max_chars {
                chunks.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Sentence boundaries: after . ! ? … (with trailing quotes/brackets) followed
/// by whitespace, or at newlines. An abbreviation heuristic is deliberately
/// absent - a false split costs one extra episode boundary at a place where a
/// reader would pause anyway, which is cheap next to a mis-merged 700-char run.
fn split_sentences(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = text[i..].chars().next().unwrap();
        let l = c.len_utf8();
        if c == '\n' {
            out.push(&text[start..i]);
            start = i + l;
        } else if matches!(c, '.' | '!' | '?' | '\u{2026}') {
            // swallow closers that belong to the sentence
            let mut j = i + l;
            while j < bytes.len() {
                let d = text[j..].chars().next().unwrap();
                if matches!(d, '"' | '\'' | ')' | ']' | '\u{201d}' | '\u{2019}') {
                    j += d.len_utf8();
                } else {
                    break;
                }
            }
            if j >= bytes.len() || text[j..].chars().next().unwrap().is_whitespace() {
                out.push(&text[start..j]);
                start = j;
            }
            i = j;
            continue;
        }
        i += l;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_wraps_text_ids_in_the_documented_specials() {
        let ids = prompt_ids(&[10, 11, 12]);
        assert_eq!(
            ids,
            vec![SOH, BOS, 10, 11, 12, TEXT_EOT, EOH, SOA, CODE_SOS]
        );
    }

    #[test]
    fn quotes_in_a_description_cannot_break_the_attribute() {
        let p = prompt_text("says \"hi\" a lot", "Hello. <laugh>");
        assert_eq!(p, "<description=\"says 'hi' a lot\"> Hello. <laugh>");
    }

    #[test]
    fn frames_unpack_to_the_1_2_4_hierarchy() {
        // one frame whose slot-local codes are 0..7
        let frame: Vec<u32> =
            (0..7).map(|j| SNAC_MIN + j * CODEBOOK + j).collect();
        let codes = unpack_frames(&frame).unwrap();
        assert_eq!(codes.l1, vec![0]);
        assert_eq!(codes.l2, vec![1, 4]);
        assert_eq!(codes.l3, vec![2, 3, 5, 6]);
        assert!(codes.ok());
    }

    #[test]
    fn a_wrong_slot_token_is_named_not_decoded() {
        // slot 1's id planted in slot 0
        let mut frame: Vec<u32> =
            (0..7).map(|j| SNAC_MIN + j * CODEBOOK).collect();
        frame[0] = SNAC_MIN + CODEBOOK;
        assert!(unpack_frames(&frame).err().unwrap().contains("slot 0"));
        // partial frames are refused, not zero-padded
        assert!(unpack_frames(&frame[..6]).is_err());
        assert!(unpack_frames(&[]).is_err());
    }

    #[test]
    fn text_is_cleaned_without_losing_emotion_tags() {
        let s = "Hello\u{7}!  <laugh> \r\n\r\n\r\n\r\nNext  line\t.";
        let c = clean_text(s);
        assert!(c.contains("<laugh>"));
        assert!(!c.contains('\u{7}'));
        assert!(!c.contains("\n\n\n"));
    }

    #[test]
    fn chunks_break_at_sentences_and_respect_the_cap() {
        let text = "First sentence. Second one is here! A third? \
                    And a fourth sentence to overflow the tiny cap.";
        let chunks = chunk_text(text, 60);
        assert!(chunks.len() >= 2, "{chunks:?}");
        assert!(chunks.iter().all(|c| c.len() <= 60), "{chunks:?}");
        // nothing lost
        let rejoined = chunks.join(" ");
        for w in ["First", "Second", "third", "fourth"] {
            assert!(rejoined.contains(w));
        }
        // sentence endings stay attached to their sentence
        assert!(matches!(chunks[0].chars().last(), Some('.' | '!' | '?')), "{chunks:?}");
    }

    #[test]
    fn a_single_monster_sentence_splits_at_words() {
        let long = "word ".repeat(50);
        let chunks = chunk_text(&long, 60);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 60));
        assert!(chunks.iter().all(|c| !c.contains("wor ")), "no mid-word cuts");
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk_text("Hello there.", 600), vec!["Hello there."]);
        assert!(chunk_text("   ", 600).is_empty());
    }

    #[test]
    fn quoted_sentence_ends_keep_their_closers() {
        let chunks = chunk_text("He said \"stop.\" Then left. Followed by more text after that.", 30);
        assert!(chunks[0].contains("\"stop.\""), "{chunks:?}");
    }
}
