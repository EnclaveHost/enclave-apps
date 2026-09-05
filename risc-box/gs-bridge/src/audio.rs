// The audio stream on :48000.
//
// The machine has a sound card now (emu/src/device/virtio_snd.rs), so this
// carries what the guest actually played: opus.rs pumps PCM out of the app's
// /audio, resamples it to 48 kHz and encodes 5 ms frames, and this packetizes
// them. When the guest is silent — or has not produced a whole frame yet —
// the stream falls back to well-formed silence rather than stopping, because
// the client brings up an Opus decoder and pings this port during the
// handshake. (A missing audio stream is not fatal to the client —
// AudioStream.c has no termination path for it — but a malformed one produces
// a decode-error log on every packet.)
//
// Framing mirrors Sunshine's audioBroadcastThread: RTP payload type 97, 4
// data shards per FEC block followed by 2 parity shards on payload type 127.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use crate::crypto;
use crate::fec;
use crate::session::Session;

const RTPA_DATA_SHARDS: usize = 4;

/// Packet duration in milliseconds, matching what we advertise.
const PACKET_DURATION_MS: u64 = 5;

fn rtp_header(payload_type: u8, seq: u16, timestamp: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = payload_type;
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    // ssrc stays 0
    h
}

pub fn run(session: Arc<Session>, app: Arc<crate::app::App>, sock: Arc<UdpSocket>) {
    let cfg = session.config.lock().unwrap().clone();
    let encrypted = cfg.audio_encrypted;

    // The pump owns /audio (taking from it is destructive), so exactly one
    // runs, alongside this thread, for the life of the session.
    let source = crate::opus::AudioSource::new();
    {
        let (s, a, src) = (session.clone(), app.clone(), source.clone());
        std::thread::spawn(move || src.pump(s, a));
    }
    let mut encoder = crate::opus::Encoder::new();
    let mut opus_buf = [0u8; 1275]; // the largest packet Opus will produce
    let silent_opus = crate::opus::silence_packet();
    let mut heard = false;
    // Real frames vs silence, reported now and then: silence while the guest
    // is playing IS the chopping, so this is the number to watch.
    let (mut real, mut filler, mut last_report) = (0u32, 0u32, std::time::Instant::now());

    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    // The FEC block's data shards, kept so parity can be generated at the end
    // of each group of four.
    let mut block: Vec<Vec<u8>> = Vec::with_capacity(RTPA_DATA_SHARDS);
    let mut base_seq: u16 = 0;
    let mut base_ts: u32 = 0;

    match encoder.is_some() {
        true => eprintln!("[audio] streaming guest audio on :{} (encrypted={encrypted})",
                          crate::session::PORT_AUDIO),
        false => eprintln!("[audio] no opus encoder; streaming silence on :{}",
                           crate::session::PORT_AUDIO),
    }

    // Pace on an ABSOLUTE deadline, not a fixed sleep after the work. The RTP
    // timestamps promise a packet every 5 ms (48 kHz), but `wait(5ms)` slept 5
    // ms ON TOP OF the encode/encrypt/FEC/send, so the true rate ran slower
    // than 200/s. The resampled ring then filled faster than this drained it,
    // overflowed its 200 ms cap, and dropped samples mid-stream — a click each
    // time, i.e. the crackle. Advancing a deadline by exactly 5 ms and sleeping
    // only the remainder locks the long-term rate at 48 kHz and lets the sleep
    // absorb the work.
    let mut deadline = std::time::Instant::now();

    while !session.is_stopping() {
        let Some(peer) = *session.audio_peer.lock().unwrap() else {
            if !session.wait(Duration::from_millis(100)) {
                break;
            }
            continue;
        };

        // A whole 5 ms frame or nothing: a partial frame would be a click,
        // and silence is what the client expects between sounds anyway.
        let frame: &[u8] = match encoder.as_mut().and_then(|e| {
            source.take_frame().and_then(|pcm| e.encode(&pcm, &mut opus_buf))
        }) {
            Some(n) => {
                if !heard {
                    heard = true;
                    eprintln!("[audio] first guest audio frame ({n} bytes opus)");
                }
                real += 1;
                &opus_buf[..n]
            }
            None => {
                filler += 1;
                &silent_opus
            }
        };
        if heard && last_report.elapsed() >= Duration::from_secs(10) {
            let total = real + filler;
            if total > 0 && real > 0 {
                eprintln!("[audio] {}% carried sound ({real} frames, {filler} silence)",
                          100 * real / total);
            }
            real = 0;
            filler = 0;
            last_report = std::time::Instant::now();
        }

        // Encrypt if the client negotiated audio encryption: AES-128-CBC with
        // IV = BE32(rikeyid + sequenceNumber) in the first four bytes.
        let payload = if encrypted {
            let mut iv = [0u8; 16];
            let iv_seq = session.riki_key_id.wrapping_add(seq as u32);
            iv[0..4].copy_from_slice(&iv_seq.to_be_bytes());
            crypto::cbc_encrypt(&session.key, &iv, frame)
        } else {
            frame.to_vec()
        };

        if seq as usize % RTPA_DATA_SHARDS == 0 {
            base_seq = seq;
            base_ts = timestamp;
            block.clear();
        }

        let mut pkt = Vec::with_capacity(12 + payload.len());
        pkt.extend_from_slice(&rtp_header(97, seq, timestamp));
        pkt.extend_from_slice(&payload);
        let _ = sock.send_to(&pkt, peer);

        block.push(payload.clone());

        // At the end of a block, emit the parity shards.
        if (seq as usize + 1) % RTPA_DATA_SHARDS == 0 && block.len() == RTPA_DATA_SHARDS {
            let refs: Vec<&[u8]> = block.iter().map(|s| s.as_slice()).collect();
            let parity = fec::encode_audio(&refs);

            for (x, shard) in parity.iter().enumerate() {
                let mut pkt = Vec::with_capacity(24 + shard.len());
                // FEC packets use payload type 127 and a zero timestamp.
                pkt.extend_from_slice(&rtp_header(127, seq.wrapping_add(x as u16 + 1), 0));
                // AUDIO_FEC_HEADER: [fecShardIndex][payloadType][baseSeq BE16][baseTs BE32][ssrc BE32]
                pkt.push(x as u8);
                pkt.push(97);
                pkt.extend_from_slice(&base_seq.to_be_bytes());
                pkt.extend_from_slice(&base_ts.to_be_bytes());
                pkt.extend_from_slice(&0u32.to_be_bytes());
                pkt.extend_from_slice(shard);
                let _ = sock.send_to(&pkt, peer);
            }
        }

        seq = seq.wrapping_add(1);
        // GameStream's audio clock counts milliseconds, not PCM samples.
        // Moonlight reconstructs missing timestamps using AudioPacketDuration.
        timestamp = timestamp.wrapping_add(PACKET_DURATION_MS as u32);

        deadline += Duration::from_millis(PACKET_DURATION_MS);
        let now = std::time::Instant::now();
        if deadline > now {
            if !session.wait(deadline - now) {
                break;
            }
        } else {
            // Fell behind — work outran the 5 ms budget, or the scheduler
            // hiccuped. Do NOT burst to catch up (that overflows the client's
            // jitter buffer); resync the cadence to now and continue.
            deadline = now;
            if session.is_stopping() {
                break;
            }
        }
    }

    eprintln!("[audio] stopped");
}
