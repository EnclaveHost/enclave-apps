// The audio stream on :48000.
//
// The emulated machine has no sound device, so there is nothing to capture.
// We still run the stream because the client brings up an Opus decoder and
// pings this port during the handshake; sending well-formed silence keeps
// that path healthy. (A missing audio stream is not fatal to the client —
// AudioStream.c has no termination path for it — but a malformed one
// produces a decode-error log on every packet.)
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
const RTPA_FEC_SHARDS: usize = 2;

/// One 5 ms stereo Opus frame of silence.
///
/// TOC byte 0xFC selects config 31 (CELT fullband), stereo, 1 frame per
/// packet; the two following bytes are a minimal empty CELT payload.
const SILENT_OPUS: [u8; 3] = [0xFC, 0xFF, 0xFE];

/// Packet duration in milliseconds, matching what we advertise.
const PACKET_DURATION_MS: u64 = 5;
/// 48 kHz Opus samples per packet.
const SAMPLES_PER_PACKET: u32 = 48_000 / 1000 * PACKET_DURATION_MS as u32;

fn rtp_header(payload_type: u8, seq: u16, timestamp: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80;
    h[1] = payload_type;
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    // ssrc stays 0
    h
}

pub fn run(session: Arc<Session>, sock: Arc<UdpSocket>) {
    let cfg = session.config.lock().unwrap().clone();
    let encrypted = cfg.audio_encrypted;

    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    // The FEC block's data shards, kept so parity can be generated at the end
    // of each group of four.
    let mut block: Vec<Vec<u8>> = Vec::with_capacity(RTPA_DATA_SHARDS);
    let mut base_seq: u16 = 0;
    let mut base_ts: u32 = 0;

    eprintln!("[audio] streaming silence on :{} (encrypted={encrypted})", crate::session::PORT_AUDIO);

    while !session.is_stopping() {
        let Some(peer) = *session.audio_peer.lock().unwrap() else {
            if !session.wait(Duration::from_millis(100)) {
                break;
            }
            continue;
        };

        // Encrypt if the client negotiated audio encryption: AES-128-CBC with
        // IV = BE32(rikeyid + sequenceNumber) in the first four bytes.
        let payload = if encrypted {
            let mut iv = [0u8; 16];
            let iv_seq = session.riki_key_id.wrapping_add(seq as u32);
            iv[0..4].copy_from_slice(&iv_seq.to_be_bytes());
            crypto::cbc_encrypt(&session.key, &iv, &SILENT_OPUS)
        } else {
            SILENT_OPUS.to_vec()
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
            let shard_len = block.iter().map(|s| s.len()).max().unwrap_or(0);
            let padded: Vec<Vec<u8>> = block
                .iter()
                .map(|s| {
                    let mut v = s.clone();
                    v.resize(shard_len, 0);
                    v
                })
                .collect();
            let refs: Vec<&[u8]> = padded.iter().map(|s| s.as_slice()).collect();
            let parity = fec::encode(&refs, RTPA_FEC_SHARDS);

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
        timestamp = timestamp.wrapping_add(SAMPLES_PER_PACKET);

        if !session.wait(Duration::from_millis(PACKET_DURATION_MS)) {
            break;
        }
    }

    eprintln!("[audio] stopped");
}
