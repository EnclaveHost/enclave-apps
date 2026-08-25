// The video stream: RISC Box desktop -> NVENC -> GameStream RTP on :47998.
//
// Three stages, mirroring Sunshine's videoBroadcastThread:
//
//   1. a feeder thread pulls raw RGB frames from the app's GET /fb.rgb and
//      writes them into ffmpeg's stdin;
//   2. ffmpeg hardware-encodes on the GPU's NVENC engine (h264_nvenc — it
//      errors out rather than falling back to CPU, so a running pipeline is
//      itself proof the GPU is doing the work) and writes Annex-B to stdout;
//   3. this module splits that into access units and packetizes each one into
//      RTP + NV_VIDEO_PACKET shards with Reed-Solomon parity.
//
// The packet layout the client expects (moonlight-common-c Video.h,
// RtpVideoQueue.c) is, per shard:
//
//   [RTP_PACKET 12][reserved 4][NV_VIDEO_PACKET 16][payload ...]
//
// with the first shard of a frame carrying an 8-byte short frame header
// ahead of the bitstream.

use std::io::{Read, Write};
use std::net::UdpSocket;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::fec;
use crate::session::Session;

/// moonlight-common-c Video.h: MAX_RTP_HEADER_SIZE.
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

pub struct Encoder {
    child: Child,
}

impl Encoder {
    /// Spawn ffmpeg reading raw RGB on stdin and emitting Annex-B H.264 on
    /// stdout, encoded on the GPU's NVENC engine.
    ///
    /// The IDR interval is pinned to ~1s so that a client IDR request (or a
    /// mid-stream join) is satisfied promptly: we cannot signal a keyframe
    /// through a pipe, so a short GOP is what bounds recovery time.
    pub fn spawn(
        cfg: &crate::session::StreamConfig,
        codec: &str,
        source: (u32, u32),
    ) -> std::io::Result<Encoder> {
        // The source is the machine's framebuffer, which has its own fixed
        // size; the client negotiates its own.
        //
        // When they differ, the scale filter runs on the CPU (the frames
        // arrive in system memory, so there is no GPU surface to scale), and
        // resampling 1024x768 to 1280x720 sixty times a second is the single
        // most expensive thing this process does — for a picture that is
        // strictly worse than the original. When they match, the filter is
        // dropped entirely rather than left in as a no-op, which is the case
        // worth aiming for: stream at the framebuffer's own size.
        let in_size = format!("{}x{}", source.0, source.1);
        let rescaling = (cfg.width, cfg.height) != (source.0, source.1);
        let scale = format!("scale={}:{}:flags=bilinear", cfg.width, cfg.height);
        let fps = cfg.fps.max(1).to_string();
        let bitrate = format!("{}k", cfg.bitrate_kbps.max(500));
        // One IDR per second by default. GSB_GOP overrides the interval (in
        // frames) for measurement rigs that want the periodic keyframe out of
        // the way; unset, behaviour is unchanged.
        let gop = std::env::var("GSB_GOP")
            .ok()
            .filter(|v| v.parse::<u32>().is_ok())
            .unwrap_or_else(|| cfg.fps.max(1).to_string());

        let mut args: Vec<&str> = vec![
            "-hide_banner", "-loglevel", "error", "-nostdin",
            "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &in_size, "-r", &fps, "-i", "-",
        ];
        if rescaling {
            eprintln!(
                "[video] rescaling {}x{} -> {}x{} on the CPU; stream at {}x{} to avoid it",
                source.0, source.1, cfg.width, cfg.height, source.0, source.1
            );
            args.extend_from_slice(&["-vf", &scale]);
        }

        let child = Command::new("ffmpeg")
            .args(args)
            .args([
                "-c:v", codec,
                "-preset", "p1",           // lowest latency NVENC preset
                "-tune", "ull",            // ultra-low-latency
                "-zerolatency", "1",
                // ffmpeg's nvenc wrapper defaults -delay to its async encode
                // depth, which HOLDS finished frames inside the encoder.
                // Measured on this exact command line: 68 ms write-to-output
                // by default, 2 ms with the delay forced to zero. "ull" does
                // not imply it; it must be explicit.
                "-delay", "0",
                // VBR rather than CBR: the emulated desktop is mostly static,
                // and CBR would pad every frame with filler data to hit the
                // target rate. Capping at the negotiated bitrate keeps us
                // inside what the client asked for.
                "-rc", "vbr",
                "-b:v", &bitrate,
                "-maxrate", &bitrate,
                "-bf", "0",                // no B-frames: they add latency and reordering
                // A single reference frame: each P-frame refers only to the one
                // before it. NVENC defaults to multiple references, and a real
                // hardware decoder (moonlight-qt on VDPAU) then throws "missing
                // reference picture" on the P-frames after a keyframe and drops
                // everything until the next one — a 2 fps slideshow. One
                // reference keeps the chain trivial and the decoder happy.
                "-refs", "1",
                "-g", &gop,
                "-forced-idr", "1",
                "-pix_fmt", "yuv420p",
                // Repeat SPS/PPS ahead of every keyframe. NVENC otherwise
                // emits them once, and the client identifies an IDR frame by
                // the frame starting with an SPS — without this, every IDR is
                // invisible to it and the stream never starts.
                "-bsf:v", "dump_extra=freq=keyframe",
                "-f", "h264", "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        Ok(Encoder { child })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Feed the encoder until the session stops.
///
/// Two sources, chosen by where the bridge is running:
///
/// * a local mirror kept current by the app's `/display` band stream
///   (`Some(screen)`), which is what makes a REMOTE bridge possible at all —
///   only changed rows cross the network, and reading a frame is a memcpy;
/// * `GET /fb.rgb` per frame otherwise, which is simplest and perfectly good
///   when the bridge sits beside the app, but fetches 2.25 MiB every time and
///   measured 2.9 seconds per frame against a deployment on the fleet.
fn feeder(
    session: Arc<Session>,
    app: Arc<App>,
    screen: Option<Arc<crate::screen::Screen>>,
    mut stdin: std::process::ChildStdin,
    fps: u32,
) {
    let interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let mut last_frame: Option<Vec<u8>> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut generation = u64::MAX;
    let (mut fed, mut fresh) = (0u64, 0u64);
    let mut reported = Instant::now();

    while !session.is_stopping() {
        let started = Instant::now();

        let frame = match &screen {
            Some(sc) => {
                // The mirror always holds a whole picture, so there is no
                // failure case here — before the first band lands it is black,
                // which is exactly what the guest's screen looked like anyway.
                //
                // A frame still goes to the encoder on every tick, because the
                // encoder's input is a fixed-rate raw stream and skipping one
                // would shift every timestamp after it. What is skipped is the
                // 2.25 MiB copy when the picture is the one we already hold.
                let now = sc.snapshot_if_changed(&mut buf, generation);
                if now != generation {
                    fresh += 1;
                    generation = now;
                }
                &buf
            }
            None => match app.get("/fb.rgb") {
                Ok(f) if !f.is_empty() => {
                    last_frame = Some(f);
                    last_frame.as_ref().unwrap()
                }
                // A failed or empty fetch repeats the previous frame rather
                // than stalling the encoder, which would stall the stream.
                _ => match &last_frame {
                    Some(f) => f,
                    None => {
                        if !session.wait(interval) {
                            break;
                        }
                        continue;
                    }
                },
            },
        };

        if stdin.write_all(frame).is_err() {
            break;
        }
        fed += 1;

        // How much of what we encode is actually new is the number that says
        // whether the bottleneck is here or upstream in the guest.
        if reported.elapsed() >= Duration::from_secs(10) {
            let secs = reported.elapsed().as_secs_f64();
            eprintln!(
                "[video] source: {:.1} new frames/s of {:.1} encoded/s",
                fresh as f64 / secs,
                fed as f64 / secs
            );
            reported = Instant::now();
            fed = 0;
            fresh = 0;
        }

        let elapsed = started.elapsed();
        if elapsed < interval && !session.wait(interval - elapsed) {
            break;
        }
    }
}

/// Split an Annex-B byte stream into access units.
///
/// A new access unit begins at the first VCL NAL (type 1 or 5) whose
/// first_mb_in_slice is 0 — that ue(v) is 0 exactly when the high bit of the
/// first byte after the NAL header is set. Crucially, any parameter sets and
/// SEI immediately preceding that slice belong to the *new* access unit, not
/// the one just finished: the client identifies an IDR frame by the frame
/// starting with an SPS (VideoDepacketizer.c isIdrFrameStart), so attaching
/// the SPS/PPS to the previous frame makes every IDR undetectable.
struct AnnexBSplitter {
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

                let (tag, ciphertext) = crate::crypto::gcm_encrypt(key, &iv, shard);

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
pub fn run(
    session: Arc<Session>,
    app: Arc<App>,
    screen: Option<Arc<crate::screen::Screen>>,
    sock: Arc<UdpSocket>,
    codec: String,
    source: (u32, u32),
) {
    let cfg = session.config.lock().unwrap().clone();

    let mut encoder = match Encoder::spawn(&cfg, &codec, source) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[video] failed to spawn ffmpeg ({codec}): {e}");
            session.stop();
            return;
        }
    };
    eprintln!(
        "[video] NVENC {codec}: {}x{} framebuffer -> {}x{}@{} {}kbps RTP",
        source.0, source.1, cfg.width, cfg.height, cfg.fps, cfg.bitrate_kbps
    );

    let stdin = encoder.child.stdin.take().unwrap();
    let mut stdout = encoder.child.stdout.take().unwrap();

    {
        let session = session.clone();
        let app = app.clone();
        let fps = cfg.fps;
        std::thread::spawn(move || feeder(session, app, screen, stdin, fps));
    }

    // The encoder's stdout is read on its own thread and handed over as
    // chunks, so this loop can WAIT WITH A DEADLINE. An Annex-B stream only
    // proves a frame complete when the next one begins, which at f fps holds
    // every finished frame for 1/f seconds. With `-delay 0` the encoder
    // writes an access unit within ~2 ms of its frame going in, so a few
    // milliseconds of pipe silence is proof enough — flush_pending then ships
    // the frame instead of waiting a frame interval for its successor.
    const FLUSH_SILENCE: Duration = Duration::from_millis(8);
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut read_buf = vec![0u8; 256 * 1024];
        loop {
            match stdout.read(&mut read_buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if chunk_tx.send(read_buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut splitter = AnnexBSplitter::new();
    let mut sink = AuSink::new(session.clone(), sock);

    while !session.is_stopping() {
        match chunk_rx.recv_timeout(FLUSH_SILENCE) {
            Ok(chunk) => {
                splitter.push(&chunk);
                while let Some(au) = splitter.next_au() {
                    if session.is_stopping() || !sink.emit(au) {
                        break;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(au) = splitter.flush_pending() {
                    sink.emit(au);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    eprintln!("[video] stream ended after {} frames", sink.frame_index);
}

/// The shared tail of both video paths: strip filler, stamp RTP time,
/// packetize with FEC and put the access unit on the wire.
struct AuSink {
    session: Arc<Session>,
    sock: Arc<UdpSocket>,
    epoch: Instant,
    /// One IV counter for the whole video stream; never reset, so no two
    /// shards are ever encrypted under the same key and nonce.
    iv_counter: std::sync::atomic::AtomicU64,
    frame_index: u32,
    lowseq: u32,
}

impl AuSink {
    fn new(session: Arc<Session>, sock: Arc<UdpSocket>) -> AuSink {
        AuSink {
            session,
            sock,
            epoch: Instant::now(),
            iv_counter: std::sync::atomic::AtomicU64::new(0),
            frame_index: 0,
            lowseq: 0,
        }
    }

    fn emit(&mut self, au: Vec<u8>) -> bool {
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
        let cipher = if cfg.encryption_flags & crate::session::SS_ENC_VIDEO != 0 {
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

/// The passthrough path: the APP encodes H.264 (minih264, inside the enclave)
/// and this bridge only repacketizes — no NVENC, no re-encode, and every
/// frame on the wire is a distinct guest frame, so the client's decode rate
/// IS the fresh-frame rate. `GET /video?codec=h264` is one SSE event per
/// access unit (base64), which also means no Annex-B splitting here.
/// The app-encoded H.264 source, held open for the BRIDGE'S WHOLE LIFE.
///
/// This used to be opened per session and torn down when the session ended.
/// That teardown is what makes a RISC Box deployment stop answering: after an
/// abandoned SSE stream the app's HTTP path stops responding to new requests
/// for minutes, so reconnecting fails and every retry holds it open. Measured
/// repeatedly -- connect, disconnect, reconnect was reliably the thing that
/// broke it, and "wait four minutes" was the only workaround.
///
/// So the stream is opened ONCE and never closed between sessions. Sessions
/// attach and detach a sink; frames arriving with no session attached are
/// dropped on the floor, which costs one SSE stream of bandwidth from an app
/// that is encoding anyway. Reconnecting now touches nothing on the app at all.
///
/// The upstream bug is still real and still worth fixing at the platform (the
/// guest is healthy and accepting throughout -- it is the hop into the tenant
/// that stalls). This removes the trigger, not the fault.
pub struct AppH264Source {
    sink: Arc<std::sync::Mutex<Option<AuSink>>>,
}

impl AppH264Source {
    /// Open the stream and keep it open. Returns immediately; the reader runs
    /// on its own thread for the life of the process.
    pub fn start(app: Arc<App>, kbps: u32) -> AppH264Source {
        let sink: Arc<std::sync::Mutex<Option<AuSink>>> = Arc::new(std::sync::Mutex::new(None));
        let reader = sink.clone();
        std::thread::spawn(move || Self::pump(app, kbps, reader));
        AppH264Source { sink }
    }

    /// A session starts: give it a fresh sink (frame numbering restarts, and
    /// the client is told to expect a new stream).
    pub fn attach(&self, session: Arc<Session>, sock: Arc<UdpSocket>) {
        *self.sink.lock().unwrap() = Some(AuSink::new(session, sock));
        eprintln!("[video] session attached to the persistent /video stream");
    }

    /// A session ends. The STREAM STAYS OPEN -- that is the entire point.
    pub fn detach(&self) {
        let framed = self.sink.lock().unwrap().take().map(|s| s.frame_index).unwrap_or(0);
        eprintln!("[video] session detached after {framed} frames (stream stays open)");
    }

    fn pump(app: Arc<App>, kbps: u32, sink: Arc<std::sync::Mutex<Option<AuSink>>>) {
        use std::io::BufRead;
        let (mut fresh, mut reported) = (0u64, Instant::now());
        let mut peer_seen = false;

        loop {
            let mut r = match app.get_stream(&format!("/video?codec=h264&kbps={kbps}")) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[video] /video connect failed: {e}; retrying");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            eprintln!("[video] persistent /video stream open (kbps={kbps})");
            let mut line = String::new();
            loop {
                // Keyframe requests are per-session state, so they are read
                // through whatever sink is currently attached.
                {
                    let mut guard = sink.lock().unwrap();
                    if let Some(s) = guard.as_mut() {
                        let has_peer = s.session.video_peer.lock().unwrap().is_some();
                        if !peer_seen && has_peer {
                            peer_seen = true;
                            s.session.request_idr();
                        }
                        if !has_peer {
                            peer_seen = false;
                        }
                        if s.session.take_idr_request() {
                            drop(guard);
                            let _ = app.post_json("/video-key", "{}");
                        }
                    }
                }

                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // the app closed it; redial
                    Ok(_) => {}
                }
                let Some(payload) = line.strip_prefix("data: ") else { continue };
                let Some(d) = crate::screen::json_str(payload.trim_end(), "d") else { continue };
                let Some(au) = crate::screen::b64_decode(d) else {
                    eprintln!("[video] frame with undecodable base64, skipped");
                    continue;
                };
                if au.is_empty() {
                    continue;
                }
                fresh += 1;
                if reported.elapsed() >= Duration::from_secs(10) {
                    // Only worth saying while someone is watching; an idle
                    // bridge would otherwise print this forever.
                    if sink.lock().unwrap().is_some() {
                        eprintln!(
                            "[video] source: {:.1} app frames/s",
                            fresh as f64 / reported.elapsed().as_secs_f64()
                        );
                    }
                    reported = Instant::now();
                    fresh = 0;
                }
                // No session attached: the frame is simply dropped. The app
                // keeps encoding either way, and holding the stream open is
                // what keeps reconnecting instant.
                let mut guard = sink.lock().unwrap();
                if let Some(s) = guard.as_mut() {
                    if !s.emit(au) {
                        // The RTP send failed, not the source: drop this
                        // session's sink and keep the stream.
                        *guard = None;
                    }
                }
            }
            eprintln!("[video] persistent /video stream ended; redialing");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_access_units_at_first_slice() {
        let mut s = AnnexBSplitter::new();
        // SPS, PPS, IDR slice, then a P slice starting the next AU.
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00]); // SPS
        stream.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE]); // PPS
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]); // IDR, first_mb=0
        stream.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x00]); // P slice, first_mb=0
        stream.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x11]); // next P slice
        s.push(&stream);

        let au1 = s.next_au().expect("first AU");
        assert!(au_is_idr(&au1), "first AU should carry the IDR slice");
        assert!(au1.windows(2).any(|w| w == [0x67, 0x42]), "SPS belongs to the IDR AU");

        let au2 = s.next_au().expect("second AU");
        assert!(!au_is_idr(&au2));
    }

    /// The client only runs its IDR detection when the access unit *starts*
    /// with an SPS, so parameter sets preceding an IDR slice must land at the
    /// head of the IDR's access unit — never appended to the previous frame.
    #[test]
    fn parameter_sets_lead_the_idr_access_unit() {
        let mut s = AnnexBSplitter::new();
        let mut stream = Vec::new();
        // A P frame, then SPS/PPS/SEI introducing an IDR, then another P.
        stream.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x00]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00]); // SPS
        stream.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x00]); // PPS
        stream.extend_from_slice(&[0, 0, 0, 1, 0x06, 0x05, 0x00]); // SEI
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]); // IDR
        stream.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x11]); // next P
        s.push(&stream);

        let p_au = s.next_au().expect("P frame");
        assert!(!au_is_idr(&p_au));
        assert_eq!(
            p_au,
            vec![0, 0, 0, 1, 0x41, 0x9A, 0x00],
            "the P frame must not absorb the following parameter sets"
        );

        let idr_au = s.next_au().expect("IDR frame");
        assert!(au_is_idr(&idr_au));
        assert_eq!(
            idr_au[4] & 0x1F,
            7,
            "the IDR access unit must begin with an SPS or the client ignores it"
        );
    }

    /// A finished frame must not wait for its successor: after encoder
    /// silence, flush_pending ships the buffered access unit whole, and the
    /// splitter state is clean for the frame that arrives later.
    #[test]
    fn flush_pending_ships_the_buffered_frame_and_resets() {
        let mut s = AnnexBSplitter::new();
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00]); // SPS
        stream.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE]); // PPS
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]); // IDR
        s.push(&stream);

        assert!(s.next_au().is_none(), "no successor yet, next_au cannot cut");
        let au = s.flush_pending().expect("a complete slice is buffered");
        assert!(au_is_idr(&au));
        assert_eq!(au[4] & 0x1F, 7, "the SPS still leads the flushed AU");
        assert!(s.flush_pending().is_none(), "nothing left after the flush");

        // The next frame flows through the normal path untouched.
        s.push(&[0, 0, 0, 1, 0x41, 0x9A, 0x00]); // P slice
        assert!(s.next_au().is_none());
        let p = s.flush_pending().expect("the P frame flushes too");
        assert!(!au_is_idr(&p));
    }

    /// Silence with no complete slice buffered must flush nothing: half an
    /// access unit on the wire is a corrupted frame, not a fast one.
    #[test]
    fn flush_pending_refuses_a_sliceless_buffer() {
        let mut s = AnnexBSplitter::new();
        s.push(&[0, 0, 0, 1, 0x67, 0x42, 0x00]); // SPS only
        assert!(s.flush_pending().is_none());
    }

    /// A filler NAL ahead of the parameter sets hides the SPS from the
    /// client's IDR check, which silently costs you every mid-stream IDR.
    #[test]
    fn filler_is_stripped_so_the_sps_leads() {
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 1, 0x0C, 0xFF, 0xFF, 0xFF]); // filler
        au.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00]); // SPS
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE]); // PPS
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]); // IDR

        let cleaned = strip_filler(&au);
        assert_eq!(cleaned[4] & 0x1F, 7, "SPS must lead once filler is gone");
        assert!(au_is_idr(&cleaned));
        assert!(
            !cleaned.windows(4).any(|w| w[3] == 0x0C && w[0] == 0 && w[1] == 0 && w[2] == 1),
            "no filler NAL should remain"
        );
    }

    #[test]
    fn stripping_keeps_a_normal_access_unit_intact() {
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1, 0x06, 0x05, 0x00]); // SEI
        au.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x11]); // P slice
        assert_eq!(strip_filler(&au), au);
    }

    /// The 24-bit streamPacketIndex must keep counting where the 16-bit RTP
    /// sequence wraps — deriving it from the u16 replayed the first 65536
    /// indexes forever and every frame after ~two minutes arrived corrupt.
    #[test]
    fn stream_packet_index_survives_the_u16_wrap() {
        let mut a = vec![0u8; 32];
        let mut b = vec![0u8; 32];
        write_nv_header(&mut a, 0, 0xFFFF, 0, 0, 1);
        write_nv_header(&mut b, 0, 0x1_0000, 0, 0, 1);
        let spi = |h: &[u8]| u32::from_le_bytes([h[16], h[17], h[18], h[19]]);
        assert_eq!(spi(&a), 0xFFFF00);
        assert_eq!(spi(&b), 0x100_0000 & 0xFFFFFF00 | 0); // 0x1000000: the next 24-bit index, not zero
        assert_eq!(spi(&b).wrapping_sub(spi(&a)), 0x100, "consecutive packets stay contiguous");
    }

    #[test]
    fn fec_info_packs_the_fields_the_client_unpacks() {
        // The client reads: index = (fecInfo & 0x3FF000) >> 12,
        // dataShards = (fecInfo & 0xFFC00000) >> 22, pct = (fecInfo & 0xFF0) >> 4.
        let (idx, shards, pct) = (7u32, 33u32, 20u32);
        let fec_info = (idx << 12) | (shards << 22) | (pct << 4);
        assert_eq!((fec_info & 0x3FF000) >> 12, idx);
        assert_eq!((fec_info & 0xFFC0_0000) >> 22, shards);
        assert_eq!((fec_info & 0xFF0) >> 4, pct);
    }

    #[test]
    fn multi_fec_blocks_byte_round_trips() {
        for blocks in 1..=4usize {
            for idx in 0..blocks {
                let b = ((idx as u8) << 4) | (((blocks - 1) as u8) << 6);
                assert_eq!(((b >> 4) & 0x3) as usize, idx);
                assert_eq!((((b >> 6) & 0x3) + 1) as usize, blocks);
            }
        }
    }
}
