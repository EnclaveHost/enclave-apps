//! Video on the wire: guest framebuffer -> NVENC -> GameStream RTP on :47998.
//!
//! Ported from the native bridge, minus everything that only existed because
//! the encoder lived in another process on another machine. Gone: the ffmpeg
//! child, the raw-frame feeder that piped it 2.25 MiB a frame, and the
//! `/video` SSE passthrough. In-guest the framebuffer is simply *there*, so
//! the path is capture -> encode -> packetise with no round trip out of the
//! enclave (PLATFORM.md §4).
//!
//! What is kept verbatim is the part Moonlight actually sees: the 32-byte NV
//! video packet header, FEC block layout, Annex-B handling and the optional
//! per-shard AES-GCM. That framing was debugged against a real client and is
//! not something to re-derive.

use std::net::UdpSocket;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use crate::gamestream::fec;
use crate::gamestream::session::Session;

const MAX_RTP_HEADER_SIZE: usize = 16;
/// sizeof(video_packet_raw_t): RTP_PACKET(12) + reserved(4) + NV_VIDEO_PACKET(16).
const VIDEO_PACKET_HEADER: usize = 32;
/// sizeof(video_short_frame_header_t).
const FRAME_HEADER: usize = 8;
/// 2 bits of multiFecBlocks means at most 4 blocks per frame.
const MAX_FEC_BLOCKS: usize = 4;
/// nanors' shard ceiling.
const DATA_SHARDS_MAX: usize = fec::DATA_SHARDS_MAX;

const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
/// RTP header bit the client asserts on every video packet.
const FLAG_EXTENSION: u8 = 0x10;

/// Percentage of parity shards to generate. Sunshine's default is 20.
const FEC_PERCENTAGE: usize = 20;

/// sizeof(ENC_VIDEO_HEADER): iv[12] + frameNumber u32 + tag[16]. Prefixed to
/// each shard when video encryption is negotiated. Deliberately a multiple of
/// 16 so the FEC block size stays one too (moonlight-common-c Video.h:12-19).
const ENC_VIDEO_HEADER: usize = 32;
pub struct AnnexBSplitter {
    buf: Vec<u8>,
    /// Whether a VCL NAL has been seen in the access unit being accumulated.
    seen_vcl: bool,
    /// Start of the run of non-VCL NALs following the last VCL, if any. This
    /// is where the next access unit begins.
    nonvcl_run: Option<usize>,
    scan: usize,
}

impl AnnexBSplitter {
    fn new() -> Self {
        AnnexBSplitter { buf: Vec::new(), seen_vcl: false, nonvcl_run: None, scan: 0 }
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Return the next complete access unit, if one has been fully seen.
    fn next_au(&mut self) -> Option<Vec<u8>> {
        while let Some((sc_pos, sc_len)) = find_start_code(&self.buf, self.scan) {
            let nal_pos = sc_pos + sc_len;
            // Need the NAL header, plus one more byte for the VCL test.
            if nal_pos + 1 >= self.buf.len() {
                self.scan = sc_pos;
                return None;
            }

            let nal_type = self.buf[nal_pos] & 0x1F;
            let is_vcl = nal_type == 1 || nal_type == 5;
            let starts_au = is_vcl && (self.buf[nal_pos + 1] & 0x80) != 0;

            if starts_au && self.seen_vcl {
                // Cut before any parameter sets / SEI that lead into this slice.
                let split = self.nonvcl_run.unwrap_or(sc_pos);
                let au = self.buf[..split].to_vec();
                self.buf.drain(..split);
                self.seen_vcl = false;
                self.nonvcl_run = None;
                self.scan = 0;
                return Some(au);
            }

            if is_vcl {
                self.seen_vcl = true;
                self.nonvcl_run = None;
            } else if self.seen_vcl && self.nonvcl_run.is_none() {
                self.nonvcl_run = Some(sc_pos);
            }

            self.scan = nal_pos;
        }
        None
    }

    /// Give up waiting for the next access unit to begin and emit what is
    /// buffered, if it already contains a complete slice.
    ///
    /// `next_au` can only cut when the FOLLOWING frame's first slice arrives,
    /// so a stream that pauses (or simply runs at 30 fps) holds every finished
    /// frame for a full frame interval. The caller invokes this after a few
    /// milliseconds of encoder silence instead: with `-delay 0` the encoder
    /// writes each access unit within ~2 ms of its frame going in, so a quiet
    /// pipe means the buffered bytes ARE the complete frame, not half of one.
    fn flush_pending(&mut self) -> Option<Vec<u8>> {
        if !self.seen_vcl || self.buf.is_empty() {
            return None;
        }
        let au = std::mem::take(&mut self.buf);
        self.seen_vcl = false;
        self.nonvcl_run = None;
        self.scan = 0;
        Some(au)
    }
}

/// Find the next Annex-B start code at or after `from`. Returns (position, length).
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 4 <= buf.len() && buf[i + 2] == 0 && buf[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

/// Remove filler-data NALs (type 12) from an access unit.
///
/// NVENC emits filler to pad a static screen up to the target bitrate. It
/// carries no picture data, and when it lands at the head of an access unit
/// it hides the SPS that the client uses to recognize an IDR frame
/// (VideoDepacketizer.c skips AUD and SEI when looking for it, but not
/// filler). Dropping it also stops us from paying real bandwidth for padding.
fn strip_filler(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len());
    let mut scan = 0usize;

    while let Some((sc_pos, sc_len)) = find_start_code(au, scan) {
        let nal_pos = sc_pos + sc_len;
        if nal_pos >= au.len() {
            break;
        }
        let nal_type = au[nal_pos] & 0x1F;
        let end = find_start_code(au, nal_pos)
            .map(|(p, _)| p)
            .unwrap_or(au.len());
        if nal_type != 12 {
            out.extend_from_slice(&au[sc_pos..end]);
        }
        scan = end;
        if end == au.len() {
            break;
        }
    }

    if out.is_empty() {
        au.to_vec()
    } else {
        out
    }
}

/// True if the access unit contains an IDR slice (NAL type 5).
fn au_is_idr(au: &[u8]) -> bool {
    let mut scan = 0usize;
    while let Some((pos, len)) = find_start_code(au, scan) {
        let np = pos + len;
        if np >= au.len() {
            break;
        }
        if au[np] & 0x1F == 5 {
            return true;
        }
        scan = np;
    }
    false
}

/// Packetize and send one access unit as a GameStream video frame.
///
/// Mirrors Sunshine stream.cpp:1493-1785 — the shard geometry, the fecInfo
/// bit packing, and the per-block SOF/EOF flags all have to match what
/// RtpVideoQueue.c reconstructs.
#[allow(clippy::too_many_arguments)]
fn send_frame(
    sock: &UdpSocket,
    peer: std::net::SocketAddr,
    au: &[u8],
    frame_index: u32,
    lowseq: &mut u32,
    timestamp: u32,
    packet_size: usize,
    min_required_fec_packets: usize,
    is_idr: bool,
    cipher: Option<(&[u8], &std::sync::atomic::AtomicU64)>,
) -> std::io::Result<()> {
    let blocksize = packet_size + MAX_RTP_HEADER_SIZE;
    let payload_blocksize = blocksize - VIDEO_PACKET_HEADER;

    // The 8-byte short frame header precedes the bitstream.
    let mut frame_header = [0u8; FRAME_HEADER];
    frame_header[0] = 0x01; // short header type
    // bytes 1..3: frame_processing_latency (LE16), left 0
    frame_header[3] = if is_idr { 2 } else { 1 };
    let last_payload_len = {
        let n = (au.len() + FRAME_HEADER) % (packet_size - 16);
        if n == 0 { (packet_size - 16) as u16 } else { n as u16 }
    };
    frame_header[4..6].copy_from_slice(&last_payload_len.to_le_bytes());

    // Build the interleaved buffer: every `blocksize` element is
    // [32 bytes reserved for headers][payload_blocksize bytes of data],
    // with the final element short. (Sunshine's concat_and_insert.)
    let data_len = FRAME_HEADER + au.len();
    let pad = data_len % payload_blocksize != 0;
    let elements = data_len / payload_blocksize + if pad { 1 } else { 0 };
    let mut buf = vec![0u8; elements * VIDEO_PACKET_HEADER + data_len];
    {
        let joined: Vec<u8> = frame_header.iter().copied().chain(au.iter().copied()).collect();
        for x in 0..elements {
            let take = if x == elements - 1 { data_len - x * payload_blocksize } else { payload_blocksize };
            let dst = x * (VIDEO_PACKET_HEADER + payload_blocksize) + VIDEO_PACKET_HEADER;
            let src = x * payload_blocksize;
            buf[dst..dst + take].copy_from_slice(&joined[src..src + take]);
        }
    }

    // Split into FEC blocks, each aligned to blocksize.
    let mut fec_percentage = FEC_PERCENTAGE;
    let max_data_shards_per_block = (DATA_SHARDS_MAX * 100) / (100 + fec_percentage);
    let max_data_per_block = max_data_shards_per_block * blocksize;
    let mut blocks_needed = (buf.len() + max_data_per_block - 1) / max_data_per_block;
    if blocks_needed > MAX_FEC_BLOCKS {
        // Enormous frame: drop FEC rather than exceed the 2-bit block count.
        fec_percentage = 0;
        blocks_needed = MAX_FEC_BLOCKS;
    }
    let blocks_needed = blocks_needed.max(1);

    let unaligned = buf.len() / blocks_needed;
    let aligned = ((unaligned + blocksize - 1) / blocksize) * blocksize;

    for block_index in 0..blocks_needed {
        let start = block_index * aligned;
        if start >= buf.len() {
            break;
        }
        let end = if block_index == blocks_needed - 1 { buf.len() } else { (start + aligned).min(buf.len()) };
        let block = &mut buf[start..end];

        let packets = (block.len() + blocksize - 1) / blocksize;

        // Fill each data shard's NV_VIDEO_PACKET header.
        for x in 0..packets {
            let off = x * blocksize;
            let hdr = &mut block[off..off + VIDEO_PACKET_HEADER];
            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if x == 0 {
                flags |= FLAG_SOF;
            }
            if x == packets - 1 {
                flags |= FLAG_EOF;
            }
            write_nv_header(hdr, frame_index, lowseq.wrapping_add(x as u32), flags, block_index, blocks_needed);
        }

        // Each data shard must be exactly `blocksize` for the RS encoder;
        // the last one is zero-padded into its own buffer.
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(packets);
        for x in 0..packets {
            let off = x * blocksize;
            let take = (block.len() - off).min(blocksize);
            let mut shard = vec![0u8; blocksize];
            shard[..take].copy_from_slice(&block[off..off + take]);
            shards.push(shard);
        }

        let data_shards = shards.len();
        let parity_shards = if fec_percentage == 0 {
            0
        } else {
            let n = (data_shards * fec_percentage + 99) / 100;
            // Small frames get bumped up to the client's requested minimum.
            n.max(min_required_fec_packets.min(DATA_SHARDS_MAX - data_shards))
        };
        let effective_percentage = if parity_shards == 0 {
            0
        } else {
            ((100 * parity_shards) / data_shards).max(fec_percentage)
        };

        // Parity is computed over the data shards *including* their headers,
        // which is why the header fields the client re-derives on recovery
        // (RTP, frameIndex, multiFecBlocks, fecInfo) are stamped only after
        // this point, while the ones it trusts from recovery (flags,
        // streamPacketIndex, multiFecFlags) were stamped before it.
        if parity_shards > 0 {
            let refs: Vec<&[u8]> = shards.iter().map(|s| s.as_slice()).collect();
            shards.extend(fec::encode(&refs, parity_shards));
        }

        let total = data_shards + parity_shards;
        let multi_fec_blocks = ((block_index as u8) << 4) | (((blocks_needed - 1) as u8) << 6);
        for (x, shard) in shards.iter_mut().enumerate().take(total) {
            let seq = lowseq.wrapping_add(x as u32);

            let fec_info: u32 =
                ((x as u32) << 12) | ((data_shards as u32) << 22) | ((effective_percentage as u32) << 4);
            shard[28..32].copy_from_slice(&fec_info.to_le_bytes());
            shard[20..24].copy_from_slice(&frame_index.to_le_bytes());
            shard[27] = multi_fec_blocks;

            // RTP header. Sequence number and timestamp are big-endian.
            // The wire sequence is the low 16 bits of the running counter —
            // it wraps every 65536 packets and the client's RTP queue expects
            // that. streamPacketIndex (write_nv_header) is the SAME counter's
            // low 24 bits, which wrap ~256x later; deriving it from the u16
            // (as this code once did) replays the first 65536 packet indexes
            // forever, and the depacketizer reads the jump backwards as an
            // unrecoverable loss — every frame after ~65K packets (about two
            // minutes at 40 fps with FEC) arrived pre-declared corrupt.
            shard[0] = 0x80 | FLAG_EXTENSION;
            shard[1] = 0x00; // packetType
            shard[2..4].copy_from_slice(&(seq as u16).to_be_bytes());
            shard[4..8].copy_from_slice(&timestamp.to_be_bytes());
            shard[8..12].copy_from_slice(&0u32.to_be_bytes()); // ssrc

            // Encrypt the finished shard, if the client negotiated it. Each
            // shard gets its own IV built the NIST SP 800-38D deterministic
            // way: a 64-bit counter in the low bytes and a fixed 'V' marking
            // this as the video stream, so the counter can never collide with
            // the control channel's use of the same key.
            if let Some((key, counter)) = cipher {
                let n = counter.fetch_add(1, Ordering::Relaxed);
                let mut iv = [0u8; 12];
                iv[0..8].copy_from_slice(&n.to_le_bytes());
                iv[11] = b'V';

                let (tag, ciphertext) = crate::gamestream::crypto::gcm_encrypt(key, &iv, shard);

                let mut packet = Vec::with_capacity(ENC_VIDEO_HEADER + ciphertext.len());
                packet.extend_from_slice(&iv);
                packet.extend_from_slice(&frame_index.to_le_bytes());
                packet.extend_from_slice(&tag);
                packet.extend_from_slice(&ciphertext);
                sock.send_to(&packet, peer)?;
                continue;
            }

            if x == 0 && std::env::var_os("GSB_DUMP_SHARD").is_some() {
                let head: Vec<String> = shard[..48.min(shard.len())]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                eprintln!("[video] shard0 frame={frame_index} len={} : {}", shard.len(), head.join(" "));
            }

            sock.send_to(shard, peer)?;
        }

        *lowseq = lowseq.wrapping_add(total as u32);
    }

    Ok(())
}

/// Write the NV_VIDEO_PACKET fields inside a shard's 32-byte header area.
///
/// Shard layout: RTP_PACKET(0..12), reserved(12..16), NV_VIDEO_PACKET(16..32),
/// where the NV header is
///   streamPacketIndex u32 LE @16, frameIndex u32 LE @20, flags @24,
///   extraFlags @25, multiFecFlags @26, multiFecBlocks @27, fecInfo u32 LE @28.
fn write_nv_header(hdr: &mut [u8], frame_index: u32, seq: u32, flags: u8, block_index: usize, blocks_needed: usize) {
    // streamPacketIndex is the low 24 bits of the running packet counter,
    // shifted left 8. The client masks the low byte off and requires the
    // 24-bit value to be contiguous across the WHOLE stream — so it must come
    // from the full counter, never from the 16-bit RTP sequence (which wraps
    // 256 times per streamPacketIndex cycle).
    let spi: u32 = seq << 8;
    hdr[16..20].copy_from_slice(&spi.to_le_bytes());
    hdr[20..24].copy_from_slice(&frame_index.to_le_bytes());
    hdr[24] = flags;
    hdr[25] = 0; // extraFlags
    hdr[26] = 0x10; // multiFecFlags, matching what Moonlight expects
    hdr[27] = ((block_index as u8) << 4) | (((blocks_needed - 1) as u8) << 6);
}

/// Run the video stream for a session until it stops.
pub struct AuSink {
    pub session: Arc<Session>,
    sock: Arc<UdpSocket>,
    epoch: Instant,
    /// One IV counter for the whole video stream; never reset, so no two
    /// shards are ever encrypted under the same key and nonce.
    iv_counter: std::sync::atomic::AtomicU64,
    pub frame_index: u32,
    lowseq: u32,
}

impl AuSink {
    pub fn new(session: Arc<Session>, sock: Arc<UdpSocket>) -> AuSink {
        AuSink {
            session,
            sock,
            epoch: Instant::now(),
            iv_counter: std::sync::atomic::AtomicU64::new(0),
            frame_index: 0,
            lowseq: 0,
        }
    }

    pub fn emit(&mut self, au: Vec<u8>) -> bool {
        let au = strip_filler(&au);
        let Some(peer) = *self.session.video_peer.lock().unwrap() else {
            // No client ping yet — nothing to send to.
            return true;
        };

        // RTP video timestamps run on a 90 kHz clock.
        let ts = (self.epoch.elapsed().as_secs_f64() * 90_000.0) as u32;
        let is_idr = au_is_idr(&au);
        if is_idr {
            self.session.idr_requested.store(false, Ordering::Release);
        }

        if is_idr || self.frame_index % 60 == 0 {
            eprintln!(
                "[video] frame {} {} {} bytes",
                self.frame_index,
                if is_idr { "IDR" } else { "P" },
                au.len()
            );
        }

        let cfg = self.session.config.lock().unwrap().clone();
        let cipher = if cfg.encryption_flags & crate::gamestream::session::SS_ENC_VIDEO != 0 {
            Some((self.session.key.as_slice(), &self.iv_counter))
        } else {
            None
        };
        if let Err(e) = send_frame(
            &self.sock,
            peer,
            &au,
            self.frame_index,
            &mut self.lowseq,
            ts,
            cfg.packet_size,
            cfg.min_required_fec_packets,
            is_idr,
            cipher,
        ) {
            eprintln!("[video] send failed: {e}");
            return false;
        }
        self.frame_index = self.frame_index.wrapping_add(1);
        true
    }
}
