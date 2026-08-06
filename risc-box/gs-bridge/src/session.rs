// Shared state for one streaming session, from /launch through teardown.
//
// The lifecycle mirrors Sunshine's: /launch mints a pending session (keys +
// ping payload + connect data), RTSP SETUP hands those tokens to the client,
// RTSP ANNOUNCE fills in the negotiated stream config and starts the workers,
// and the control channel's ENet connect binds the peer.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

pub const PORT_HTTP: u16 = 47989;
pub const PORT_HTTPS: u16 = 47984;
pub const PORT_RTSP: u16 = 48010;
pub const PORT_VIDEO: u16 = 47998;
pub const PORT_CONTROL: u16 = 47999;
pub const PORT_AUDIO: u16 = 48000;

/// The GFE version we impersonate. The negative 4th component is what makes
/// moonlight-common-c's IS_SUNSHINE() true, which turns on the encrypted
/// control stream, multi-FEC, and the control/13/0 stream id.
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";

/// SS_ENC_* bits (moonlight-common-c Limelight-internal.h:48-50).
pub const SS_ENC_CONTROL_V2: u32 = 0x01;
pub const SS_ENC_VIDEO: u32 = 0x02;
pub const SS_ENC_AUDIO: u32 = 0x04;

/// Negotiated stream parameters, parsed from the client's RTSP ANNOUNCE.
#[derive(Clone, Debug)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub packet_size: usize,
    pub bitrate_kbps: u32,
    pub min_required_fec_packets: usize,
    pub encryption_flags: u32,
    pub audio_encrypted: bool,
    pub control_protocol_type: u32,
    pub ml_feature_flags: u32,
    pub video_format: u32, // 0 = H.264, 1 = HEVC, 2 = AV1
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            width: 1280,
            height: 720,
            fps: 60,
            packet_size: 1024,
            bitrate_kbps: 10_000,
            min_required_fec_packets: 2,
            encryption_flags: 0,
            audio_encrypted: false,
            control_protocol_type: 13,
            ml_feature_flags: 0,
            video_format: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Created by /launch, waiting for the RTSP handshake.
    Pending,
    /// ANNOUNCE done; video/audio/control workers running.
    Running,
    /// Tearing down.
    Stopping,
}

pub struct Session {
    pub id: u32,
    /// AES-128 master key: the raw `rikey` bytes from /launch.
    pub key: Vec<u8>,
    /// `rikeyid` — the audio IV seed (avRiKeyId).
    pub riki_key_id: u32,
    /// 16 hex chars echoed in RTSP SETUP as X-SS-Ping-Payload; the client
    /// puts it in its UDP pings so we can identify its video/audio ports.
    pub ping_payload: String,
    /// Random u32 echoed as X-SS-Connect-Data and presented back in the
    /// client's ENet connect data.
    pub connect_data: u32,
    pub app_id: i32,

    pub state: Mutex<State>,
    pub config: Mutex<StreamConfig>,

    /// Peer addresses learned from the client's UDP ping packets.
    pub video_peer: Mutex<Option<SocketAddr>>,
    pub audio_peer: Mutex<Option<SocketAddr>>,

    /// Outbound control-stream sequence counter (our own IV namespace).
    pub control_seq: AtomicU32,
    /// Set when the client asks for an IDR frame.
    pub idr_requested: AtomicBool,
    /// Bumped so the video worker can notice new peers/teardown promptly.
    pub generation: AtomicU64,

    /// Pointer position in normalized [0,1] coordinates. The emulated HID is
    /// an absolute pointer, so relative mouse motion has to be integrated
    /// here before it can be injected.
    pub cursor: Mutex<(f64, f64)>,

    stop_flag: AtomicBool,
    stop_cv: Condvar,
    stop_mutex: Mutex<()>,
}

impl Session {
    pub fn new(id: u32, key: Vec<u8>, riki_key_id: u32, ping_payload: String, connect_data: u32, app_id: i32) -> Session {
        Session {
            id,
            key,
            riki_key_id,
            ping_payload,
            connect_data,
            app_id,
            state: Mutex::new(State::Pending),
            config: Mutex::new(StreamConfig::default()),
            video_peer: Mutex::new(None),
            audio_peer: Mutex::new(None),
            control_seq: AtomicU32::new(0),
            idr_requested: AtomicBool::new(true),
            generation: AtomicU64::new(0),
            cursor: Mutex::new((0.5, 0.5)),
            stop_flag: AtomicBool::new(false),
            stop_cv: Condvar::new(),
            stop_mutex: Mutex::new(()),
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.stop_flag.load(Ordering::Acquire)
    }

    /// Signal every worker to wind down and wake anyone sleeping on `wait`.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        *self.state.lock().unwrap() = State::Stopping;
        let _g = self.stop_mutex.lock().unwrap();
        self.stop_cv.notify_all();
    }

    /// Sleep for `dur` unless the session stops first. Returns false if the
    /// session is stopping, so callers can use it directly as a loop guard.
    pub fn wait(&self, dur: std::time::Duration) -> bool {
        if self.is_stopping() {
            return false;
        }
        let g = self.stop_mutex.lock().unwrap();
        let (_g, _t) = self.stop_cv.wait_timeout(g, dur).unwrap();
        !self.is_stopping()
    }

    pub fn request_idr(&self) {
        self.idr_requested.store(true, Ordering::Release);
    }

    pub fn take_idr_request(&self) -> bool {
        self.idr_requested.swap(false, Ordering::AcqRel)
    }
}
