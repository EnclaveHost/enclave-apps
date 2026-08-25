//! Audio on the wire: Opus over GameStream RTP on :48000, with FEC.
//!
//! Ported from the native bridge and turned inside out for the guest. The
//! bridge parked a thread here and paced itself with `sleep`; there are no
//! threads in the sandbox, so [`AudioSender::tick`] is called from the app's
//! turn and paces on an absolute deadline it checks rather than sleeps on.
//!
//! **What is not here yet: the Opus encoder.** Moonlight's audio is Opus at
//! 48 kHz and the bridge linked the system libopus — C, and not available to a
//! wasm guest; there is no mature pure-Rust Opus encoder to put in its place.
//! Vendoring libopus (~40k lines of autotools C) is the remaining piece. Until
//! then this streams correctly-framed silence, which is a real state the
//! protocol has: the client gets a well-formed audio stream, timed and
//! FEC-protected, that happens to carry nothing. The bridge had the same
//! fallback for a host without an encoder.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::gamestream::crypto;
use crate::gamestream::fec;
use crate::gamestream::session::Session;

const RTPA_DATA_SHARDS: usize = 4;
const RTPA_FEC_SHARDS: usize = 2;

/// A single Opus frame of silence. Three bytes, and the client understands it
/// as "nothing is playing" rather than as a gap.
const SILENT_OPUS: [u8; 3] = [0xFC, 0xFF, 0xFE];

const PACKET_DURATION_MS: u64 = 5;
const SAMPLES_PER_PACKET: u32 = 48_000 / 1000 * PACKET_DURATION_MS as u32;

fn rtp_header(payload_type: u8, seq: u16, timestamp: u32) -> [u8; 12] {
    let mut h = [0u8; 12];
    h[0] = 0x80; // version 2
    h[1] = payload_type;
    h[2..4].copy_from_slice(&seq.to_be_bytes());
    h[4..8].copy_from_slice(&timestamp.to_be_bytes());
    // ssrc stays zero, as the client expects
    h
}

pub struct AudioSender {
    seq: u16,
    timestamp: u32,
    block: Vec<Vec<u8>>,
    base_seq: u16,
    base_ts: u32,
    deadline: Instant,
    announced: bool,
}

impl Default for AudioSender {
    fn default() -> Self {
        AudioSender {
            seq: 0,
            timestamp: 0,
            block: Vec::with_capacity(RTPA_DATA_SHARDS),
            base_seq: 0,
            base_ts: 0,
            deadline: Instant::now(),
            announced: false,
        }
    }
}

impl AudioSender {
    /// Send whatever 5 ms frames are due. Returns true if anything went out.
    ///
    /// Paced on an ABSOLUTE deadline: the RTP timestamps promise a packet
    /// every 5 ms, and pacing by "sleep 5 ms after the work" runs slower than
    /// that, which the client hears as crackle when its jitter buffer drains.
    /// Behind schedule it resyncs rather than bursting -- a burst overflows the
    /// client's buffer, which sounds no better.
    pub fn tick(&mut self, session: &Arc<Session>, sock: &UdpSocket) -> bool {
        let Some(peer) = *session.audio_peer.lock().unwrap() else { return false };
        let cfg = session.config.lock().unwrap().clone();
        let encrypted = cfg.audio_encrypted;

        if !self.announced {
            self.announced = true;
            eprintln!(
                "[audio] streaming on :{} (encrypted={encrypted}) - silence until an \
                 in-guest Opus encoder exists",
                crate::gamestream::session::PORT_AUDIO
            );
            self.deadline = Instant::now();
        }

        let mut sent = false;
        // Catch up at most a few frames per turn: the turn cadence is coarser
        // than 5 ms, so a little batching is expected, but unbounded catch-up
        // would burst.
        for _ in 0..8 {
            if Instant::now() < self.deadline {
                break;
            }
            self.send_one(session, sock, peer, encrypted);
            sent = true;
            self.deadline += Duration::from_millis(PACKET_DURATION_MS);
            if self.deadline < Instant::now() - Duration::from_millis(100) {
                // Fell far behind (the turn ran long). Resync rather than
                // trying to replay the gap.
                self.deadline = Instant::now();
                break;
            }
        }
        sent
    }

    fn send_one(
        &mut self,
        session: &Arc<Session>,
        sock: &UdpSocket,
        peer: std::net::SocketAddr,
        encrypted: bool,
    ) {
        let frame: &[u8] = &SILENT_OPUS;

        // AES-128-CBC with IV = BE32(rikeyid + sequenceNumber) in the first
        // four bytes, when the client negotiated audio encryption.
        let payload = if encrypted {
            let mut iv = [0u8; 16];
            let iv_seq = session.riki_key_id.wrapping_add(self.seq as u32);
            iv[0..4].copy_from_slice(&iv_seq.to_be_bytes());
            crypto::cbc_encrypt(&session.key, &iv, frame)
        } else {
            frame.to_vec()
        };

        if self.seq as usize % RTPA_DATA_SHARDS == 0 {
            self.base_seq = self.seq;
            self.base_ts = self.timestamp;
            self.block.clear();
        }

        let mut pkt = Vec::with_capacity(12 + payload.len());
        pkt.extend_from_slice(&rtp_header(97, self.seq, self.timestamp));
        pkt.extend_from_slice(&payload);
        let _ = sock.send_to(&pkt, peer);
        self.block.push(payload);

        // At the end of a block, emit the parity shards.
        if (self.seq as usize + 1) % RTPA_DATA_SHARDS == 0
            && self.block.len() == RTPA_DATA_SHARDS
        {
            let shard_len = self.block.iter().map(|s| s.len()).max().unwrap_or(0);
            let padded: Vec<Vec<u8>> = self
                .block
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
                pkt.extend_from_slice(&rtp_header(127, self.seq.wrapping_add(x as u16 + 1), 0));
                // [fecShardIndex][payloadType][baseSeq BE16][baseTs BE32][ssrc BE32]
                pkt.push(x as u8);
                pkt.push(97);
                pkt.extend_from_slice(&self.base_seq.to_be_bytes());
                pkt.extend_from_slice(&self.base_ts.to_be_bytes());
                pkt.extend_from_slice(&0u32.to_be_bytes());
                pkt.extend_from_slice(shard);
                let _ = sock.send_to(&pkt, peer);
            }
        }

        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(SAMPLES_PER_PACKET);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RTP is big-endian and version 2; the client parses these offsets
    /// directly, so a byte out of place is a stream it silently ignores.
    #[test]
    fn the_rtp_header_is_well_formed() {
        let h = rtp_header(97, 0x1234, 0xAABBCCDD);
        assert_eq!(h[0], 0x80, "version 2, no padding/extension/CSRC");
        assert_eq!(h[1], 97);
        assert_eq!(&h[2..4], &[0x12, 0x34], "sequence is big-endian");
        assert_eq!(&h[4..8], &[0xAA, 0xBB, 0xCC, 0xDD], "timestamp is big-endian");
        assert_eq!(&h[8..12], &[0, 0, 0, 0], "ssrc is zero");
    }

    /// 5 ms at 48 kHz is 240 samples; the timestamp advances by that per
    /// packet or the client's clock drifts against ours.
    #[test]
    fn the_timestamp_step_matches_five_milliseconds() {
        assert_eq!(SAMPLES_PER_PACKET, 240);
    }

    /// The silent frame is what goes out until there is an encoder, so it had
    /// better be the three bytes Opus defines rather than an empty payload.
    #[test]
    fn the_silence_frame_is_a_real_opus_packet() {
        assert_eq!(SILENT_OPUS.len(), 3);
        assert_eq!(SILENT_OPUS[0], 0xFC);
    }
}
