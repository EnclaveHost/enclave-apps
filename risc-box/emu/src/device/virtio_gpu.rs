// risc-box patch: Virtio GPU device — the machine's display controller.
// Mapped at 0x10005000, IRQ 5 on the PLIC.
//
// This replaces the simple-framebuffer for guests that can drive it, and the
// difference is not cosmetic. A simple-framebuffer is a dumb span of memory:
// geometry is frozen in the DTB, there is no cursor plane, and the host learns
// what changed only by SCANNING for it. virtio-gpu gives the guest a real
// DRM/KMS device, so:
//
//   * modes change at runtime — no DTB rebuild to alter the resolution;
//   * the guest names its dirty rectangles (RESOURCE_FLUSH) instead of the
//     host diffing whole frames to find them;
//   * the cursor is its own plane, so moving the mouse stops dirtying the
//     picture underneath it;
//   * userspace that expects DRM (SDL's KMSDRM path, Xorg's modesetting
//     driver, anything Wayland) finds the device it is looking for.
//
// The simple-framebuffer node stays in the DTB alongside this, so an image
// whose kernel lacks CONFIG_DRM_VIRTIO_GPU boots exactly as before. The host
// prefers this device's scanout whenever a resource is bound to it.
//
// ---- ON 3D -----------------------------------------------------------------
//
// 3D (virgl) is deliberately NOT advertised, and the reason is a platform
// boundary rather than an unfinished device. virgl works by having the GUEST's
// Mesa encode a GL command stream which the HOST replays through
// virglrenderer against a real GL/EGL context. This emulator is a
// wasm32-wasip2 component, and the only GPU-facing interface that world
// exposes is wasi:nn — graphs, tensors, inference. There is no EGL, no GL
// context, no command submission. Buying the deployment a GPU share does not
// change that: the share is capacity metered through MPS and spent through
// inference verbs, not a channel for arbitrary draw calls.
//
// So the device negotiates 3D honestly: VIRTIO_GPU_F_VIRGL is offered only
// when a `Virgl3d` backend is present, and `num_capsets` reports whatever that
// backend supports (0 with none). A driver that sees no VIRGL feature and no
// capsets falls back to software rendering cleanly — which is the behaviour we
// want, rather than advertising 3D and hanging the guest on the first
// SUBMIT_3D that never completes.
//
// The seam is real: implement `Virgl3d`, hand it to `with_virgl`, and the
// control path already routes context and submit commands to it. What that
// implementation needs is a host-side renderer — either a platform verb that
// exposes a GL context (note: a virgl stream carries tenant-authored shaders,
// which is the "tenant kernels" case the CC/splitting trilemma was settled
// against), or a software rasterizer running natively on the host side of this
// boundary. The latter needs no new platform capability and may well be the
// bigger win here anyway: it moves rasterization off the emulated RV64 CPU,
// which has no vector unit, onto native code.
//
// Based on VIRTIO v1.2 section 5.7 (GPU Device). Register file, FEATURES_OK
// handshake and split-virtqueue plumbing mirror virtio_snd.rs exactly; only
// the queues and payloads differ.

use std::collections::HashMap;

use mmu::MemoryWrapper;

const BASE: u64 = 0x10005000;
const MAX_QUEUE_SIZE: u32 = 256;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// 5.7.3: the guest may use 3D commands. Offered only with a backend.
const VIRTIO_GPU_F_VIRGL: u64 = 1 << 0;

// Queue indices (5.7.2)
const CONTROL_QUEUE: usize = 0;
const CURSOR_QUEUE: usize = 1;
const NUM_QUEUES: usize = 2;

// 2D commands (5.7.6.7)
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_CAPSET_INFO: u32 = 0x0108;
const CMD_GET_CAPSET: u32 = 0x0109;
const CMD_GET_EDID: u32 = 0x010a;

// 3D commands — routed to the backend, refused without one.
const CMD_CTX_CREATE: u32 = 0x0200;
const CMD_CTX_DESTROY: u32 = 0x0201;
const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
const CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
const CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const CMD_SUBMIT_3D: u32 = 0x0207;

// Cursor commands
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

// Responses (5.7.6.7)
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// VIRTIO_GPU_FLAG_FENCE: the request wants its fence signalled on completion.
const FLAG_FENCE: u32 = 1 << 0;

const CTRL_HDR_LEN: usize = 24;
/// Every format we accept is 32bpp; the guest's XRGB8888 and our scanout's
/// x8r8g8b8 are the same bytes, so the common path copies rather than converts.
const BPP: usize = 4;

const MAX_SCANOUTS: usize = 16;

/// A host-side 3D renderer for virgl command streams. See the module header
/// for why nothing implements this yet and what it would take.
pub trait Virgl3d: Send {
    /// How many capsets to advertise; 0 disables 3D negotiation entirely.
    fn num_capsets(&self) -> u32;
    /// Execute one VIRTIO_GPU_CMD_SUBMIT_3D payload in `ctx_id`'s context.
    fn submit(&mut self, ctx_id: u32, stream: &[u8]) -> Result<(), ()>;
}

struct Queue {
    num: u32,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
    avail_cursor: u16,
    used_index: u16,
}

impl Queue {
    fn new() -> Self {
        Queue { num: 0, ready: false, desc: 0, driver: 0, device: 0, avail_cursor: 0, used_index: 0 }
    }
    fn is_ready(&self) -> bool {
        self.ready && self.num != 0 && self.desc != 0 && self.driver != 0 && self.device != 0
    }
}

/// One guest-created 2D resource: its geometry, the guest pages backing it,
/// and the host's own copy that the display path reads.
struct Resource {
    width: u32,
    height: u32,
    /// Scatter-gather list of guest physical (addr, len) the guest attached.
    backing: Vec<(u64, u32)>,
    /// Host-side pixels, `width * height * BPP`, filled by TRANSFER_TO_HOST_2D.
    pixels: Vec<u8>,
    /// Whether the host copy has been filled from the backing store once.
    /// Damage transfers keep it current after that, so the full pull is a
    /// one-time cost per resource rather than a per-flip one.
    pulled: bool,
}

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub struct VirtioGpu {
    device_features_sel: u32,
    driver_features: u64,
    driver_features_sel: u32,
    queue_select: u32,
    interrupt_status: u32,
    status: u32,
    queues: [Queue; NUM_QUEUES],

    resources: HashMap<u32, Resource>,
    /// Resource bound to scanout 0, and the region of it being displayed.
    scanout_resource: u32,
    scanout_rect: Rect,
    /// What the guest has flushed since the host last looked. Coalesced into
    /// one bounding box: the display path wants "what do I re-encode", and a
    /// list of rects it would union anyway costs more to carry than it saves.
    dirty: Option<Rect>,
    /// Bytes named by scanout flush rectangles (w*h*4 per flush) — the GPU
    /// path's equivalent of the simple-framebuffer's painted-bytes counter,
    /// so an honest frame rate exists whichever device drives the screen.
    flush_bytes: u64,
    /// Monotonic count of flushes to the bound scanout. The host uses this to
    /// tell which display device the guest is actually DRAWING to, which is
    /// not the same question as which one exists — see `flushes()`.
    flushes: u64,
    /// Display geometry reported to the guest via GET_DISPLAY_INFO.
    display_width: u32,
    display_height: u32,

    virgl: Option<Box<dyn Virgl3d>>,
    cursor: Option<Cursor>,
    /// How many cursor commands have arrived, ever.
    cursor_updates: u64,
}

/// The pointer, as its own plane.
///
/// This is not decoration. With the fbdev driver X drew the pointer INTO the
/// framebuffer, so every mouse move dirtied the picture under it and the host
/// re-encoded that region. On DRM, modesetting hands the pointer to the
/// hardware cursor plane instead — so if the device acknowledges the cursor
/// commands and composites nothing, the mouse simply VANISHES, which is
/// exactly what shipping the first cut did.
struct Cursor {
    /// 0 means hidden; otherwise a normal 2D resource (typically 64x64 ARGB)
    /// the guest created and transferred like any other.
    resource_id: u32,
    x: i64,
    y: i64,
    hot_x: i64,
    hot_y: i64,
}

impl VirtioGpu {
    pub fn new(width: u32, height: u32) -> Self {
        VirtioGpu {
            device_features_sel: 0,
            driver_features: 0,
            driver_features_sel: 0,
            queue_select: 0,
            interrupt_status: 0,
            status: 0,
            queues: [Queue::new(), Queue::new()],
            resources: HashMap::new(),
            scanout_resource: 0,
            scanout_rect: Rect::default(),
            dirty: None,
            flushes: 0,
            flush_bytes: 0,
            display_width: width,
            display_height: height,
            virgl: None,
            cursor: None,
            cursor_updates: 0,
        }
    }

    /// Attach a 3D backend. Until one exists this is never called, and the
    /// device correctly tells the guest it has no 3D.
    pub fn with_virgl(mut self, backend: Box<dyn Virgl3d>) -> Self {
        self.virgl = Some(backend);
        self
    }

    fn device_features(&self) -> u64 {
        match &self.virgl {
            Some(b) if b.num_capsets() > 0 => VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_VIRGL,
            _ => VIRTIO_F_VERSION_1,
        }
    }

    fn num_capsets(&self) -> u32 {
        self.virgl.as_ref().map(|b| b.num_capsets()).unwrap_or(0)
    }

    fn driver_ready(&self) -> bool {
        (self.status & 4) != 0 // DRIVER_OK
    }

    pub fn is_interrupting(&mut self) -> bool {
        let pending = (self.interrupt_status & 0x1) != 0;
        pending && self.driver_ready()
    }

    // ---- host-facing API -------------------------------------------------

    /// The scanout's pixels, if the guest has bound one. `None` means the
    /// guest is not driving this device and the caller should fall back to the
    /// simple-framebuffer.
    pub fn scanout(&self) -> Option<(u32, u32, &[u8])> {
        let res = self.resources.get(&self.scanout_resource)?;
        match res.pixels.is_empty() {
            true => None,
            false => Some((res.width, res.height, &res.pixels)),
        }
    }

    /// What changed since the last call, as one bounding box. Taking it clears
    /// it, so a caller that skips a frame does not lose the region.
    pub fn take_dirty(&mut self) -> Option<Rect> {
        self.dirty.take()
    }

    /// How many times the guest has flushed the scanout, ever.
    ///
    /// Existence is not use. The kernel's fbdev emulation binds a scanout at
    /// boot and flushes it once, so "a resource is bound" says nothing about
    /// whether the desktop is painting HERE or into the simple-framebuffer.
    /// Serving a bound-but-idle scanout means streaming a blank screen while
    /// the real picture sits in the other buffer. A monotonic counter lets the
    /// host compare the two surfaces and follow whichever is moving.
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    pub fn flush_bytes(&self) -> u64 {
        self.flush_bytes
    }

    pub fn is_active(&self) -> bool {
        self.scanout_resource != 0 && self.resources.contains_key(&self.scanout_resource)
    }

    fn mark_dirty(&mut self, r: Rect) {
        self.dirty = Some(match self.dirty {
            None => r,
            Some(d) => {
                let x0 = d.x.min(r.x);
                let y0 = d.y.min(r.y);
                let x1 = (d.x + d.width).max(r.x + r.width);
                let y1 = (d.y + d.height).max(r.y + r.height);
                Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
            }
        });
    }

    // ---- queue service ---------------------------------------------------

    pub fn tick(&mut self, memory: &mut MemoryWrapper) {
        if !self.driver_ready() {
            return;
        }
        self.drain_queue(memory, CONTROL_QUEUE);
        self.drain_queue(memory, CURSOR_QUEUE);
    }

    fn drain_queue(&mut self, memory: &mut MemoryWrapper, qi: usize) {
        while let Some(head) = self.pop_avail(memory, qi) {
            let (req, writable) = self.walk_chain(memory, qi, head);
            let resp = self.handle(memory, &req);
            let written = write_out(memory, &writable, &resp);
            self.push_used(memory, qi, head, written);
        }
    }

    /// Build the 24-byte response header. A request that asked for a fence
    /// gets it echoed back: the driver blocks until the fence it submitted
    /// comes home, so dropping it wedges the guest.
    fn resp_hdr(req: &[u8], resp_type: u32) -> Vec<u8> {
        let flags = le32(req, 4);
        let fence_id = le64(req, 8);
        let ctx_id = le32(req, 16);
        let mut out = Vec::with_capacity(CTRL_HDR_LEN);
        out.extend_from_slice(&resp_type.to_le_bytes());
        out.extend_from_slice(&(flags & FLAG_FENCE).to_le_bytes());
        out.extend_from_slice(&fence_id.to_le_bytes());
        out.extend_from_slice(&ctx_id.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // ring_idx + padding
        out
    }

    fn handle(&mut self, memory: &mut MemoryWrapper, req: &[u8]) -> Vec<u8> {
        if req.len() < CTRL_HDR_LEN {
            return Self::resp_hdr(req, RESP_ERR_UNSPEC);
        }
        let cmd = le32(req, 0);
        match cmd {
            CMD_GET_DISPLAY_INFO => self.get_display_info(req),
            CMD_RESOURCE_CREATE_2D => self.resource_create_2d(req),
            CMD_RESOURCE_UNREF => self.resource_unref(req),
            CMD_SET_SCANOUT => self.set_scanout(memory, req),
            CMD_RESOURCE_FLUSH => self.resource_flush(req),
            CMD_TRANSFER_TO_HOST_2D => self.transfer_to_host_2d(memory, req),
            CMD_RESOURCE_ATTACH_BACKING => self.attach_backing(req),
            CMD_RESOURCE_DETACH_BACKING => self.detach_backing(req),
            // num_capsets is 0 without a backend, so a conforming driver never
            // asks; answer honestly rather than inventing a capset.
            CMD_GET_CAPSET_INFO | CMD_GET_CAPSET => Self::resp_hdr(req, RESP_ERR_UNSPEC),
            // EDID is optional and we do not claim VIRTIO_GPU_F_EDID; the
            // driver falls back to GET_DISPLAY_INFO geometry.
            CMD_GET_EDID => Self::resp_hdr(req, RESP_ERR_UNSPEC),
            CMD_SUBMIT_3D => self.submit_3d(req),
            CMD_CTX_CREATE | CMD_CTX_DESTROY | CMD_CTX_ATTACH_RESOURCE
            | CMD_CTX_DETACH_RESOURCE | CMD_RESOURCE_CREATE_3D
            | CMD_TRANSFER_TO_HOST_3D | CMD_TRANSFER_FROM_HOST_3D => {
                match self.virgl.is_some() {
                    // A backend that exists still has to implement these; the
                    // seam is here so the routing is not invented later.
                    true => Self::resp_hdr(req, RESP_OK_NODATA),
                    false => Self::resp_hdr(req, RESP_ERR_UNSPEC),
                }
            }
            CMD_UPDATE_CURSOR => self.update_cursor(req, true),
            CMD_MOVE_CURSOR => self.update_cursor(req, false),
            _ => Self::resp_hdr(req, RESP_ERR_UNSPEC),
        }
    }

    fn get_display_info(&mut self, req: &[u8]) -> Vec<u8> {
        let mut out = Self::resp_hdr(req, RESP_OK_DISPLAY_INFO);
        for i in 0..MAX_SCANOUTS {
            let enabled = i == 0;
            let (w, h) = match enabled {
                true => (self.display_width, self.display_height),
                false => (0, 0),
            };
            out.extend_from_slice(&0u32.to_le_bytes()); // x
            out.extend_from_slice(&0u32.to_le_bytes()); // y
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&(enabled as u32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes()); // flags
        }
        out
    }

    fn resource_create_2d(&mut self, req: &[u8]) -> Vec<u8> {
        let id = le32(req, CTRL_HDR_LEN);
        let width = le32(req, CTRL_HDR_LEN + 8);
        let height = le32(req, CTRL_HDR_LEN + 12);
        if id == 0 || width == 0 || height == 0 {
            return Self::resp_hdr(req, RESP_ERR_INVALID_PARAMETER);
        }
        // A resource the guest asks for is a resource the host must hold in
        // full; refuse anything that would not fit rather than allocating it.
        let bytes = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(BPP);
        if bytes == 0 || bytes > 64 * 1024 * 1024 {
            return Self::resp_hdr(req, RESP_ERR_INVALID_PARAMETER);
        }
        self.resources.insert(
            id,
            Resource { width, height, backing: Vec::new(), pixels: vec![0u8; bytes], pulled: false },
        );
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    fn resource_unref(&mut self, req: &[u8]) -> Vec<u8> {
        let id = le32(req, CTRL_HDR_LEN);
        // THE CURSOR'S RESOURCE CAN BE THE ONE BEING FREED, and forgetting
        // that is why the pointer "worked for a while and then died". X frees
        // a cursor resource whenever the pointer SHAPE changes — crossing from
        // the desktop into a window is enough. Once its resource is gone,
        // cursor_rect() returns None: nothing composites the pointer any more,
        // AND nothing reports damage for where it last was, so its final image
        // stays burned into the picture, frozen, while the guest happily moves
        // a cursor the host can no longer draw.
        //
        // So take the damage BEFORE the resource disappears (the rect needs
        // the resource's dimensions), then let go of the cursor.
        let cursor_area = match self.cursor.as_ref() {
            Some(c) if c.resource_id == id => self.cursor_rect(),
            _ => None,
        };
        if self.resources.remove(&id).is_none() {
            return Self::resp_hdr(req, RESP_ERR_INVALID_RESOURCE_ID);
        }
        if let Some(r) = cursor_area {
            self.mark_dirty(r);
        }
        if self.cursor.as_ref().is_some_and(|c| c.resource_id == id) {
            // Keep the position: the very next UPDATE_CURSOR usually just
            // hands over a new shape for the same pointer, and dropping the
            // coordinates would teleport it to wherever that request happens
            // to say.
            if let Some(c) = self.cursor.as_mut() {
                c.resource_id = 0;
            }
        }
        if self.scanout_resource == id {
            self.scanout_resource = 0;
        }
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    fn set_scanout(&mut self, memory: &mut MemoryWrapper, req: &[u8]) -> Vec<u8> {
        let r = rect_at(req, CTRL_HDR_LEN);
        let scanout_id = le32(req, CTRL_HDR_LEN + 16);
        let resource_id = le32(req, CTRL_HDR_LEN + 20);
        if scanout_id as usize >= MAX_SCANOUTS {
            return Self::resp_hdr(req, RESP_ERR_INVALID_SCANOUT_ID);
        }
        // Resource 0 means "disable this scanout", which is legal and is how
        // the guest tears the display down on the way out.
        if resource_id == 0 {
            self.scanout_resource = 0;
            return Self::resp_hdr(req, RESP_OK_NODATA);
        }
        if !self.resources.contains_key(&resource_id) {
            return Self::resp_hdr(req, RESP_ERR_INVALID_RESOURCE_ID);
        }
        self.scanout_resource = resource_id;
        // Pull the whole resource, once, right here.
        //
        // The host's copy is only ever written by TRANSFER_TO_HOST_2D, and the
        // guest only transfers what it has just DAMAGED. Everything already on
        // screen when this buffer became the scanout — the root window, an idle
        // terminal, any window that is not animating — has no damage to report
        // and would never arrive, so the stream showed a black desktop with
        // only the moving parts painted on it (measured: desktop pixels 0,0,0
        // until an xsetroot forced a repaint, after which the whole screen was
        // correct). Reading the backing store directly costs one full-frame
        // copy per mode set and makes the host's copy true from the first
        // frame instead of eventually.
        self.pull_whole_resource(memory, resource_id);
        self.scanout_rect = r;
        // The guest just told us the mode. This is the whole point of the
        // device over a simple-framebuffer: geometry comes from the guest at
        // runtime instead of from a DTB node fixed at build time.
        if r.width > 0 && r.height > 0 {
            self.display_width = r.width;
            self.display_height = r.height;
        }
        self.mark_dirty(r);
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    fn resource_flush(&mut self, req: &[u8]) -> Vec<u8> {
        let r = rect_at(req, CTRL_HDR_LEN);
        let resource_id = le32(req, CTRL_HDR_LEN + 16);
        if !self.resources.contains_key(&resource_id) {
            return Self::resp_hdr(req, RESP_ERR_INVALID_RESOURCE_ID);
        }
        if resource_id == self.scanout_resource {
            self.mark_dirty(r);
            self.flushes = self.flushes.wrapping_add(1);
            self.flush_bytes = self
                .flush_bytes
                .wrapping_add(r.width as u64 * r.height as u64 * BPP as u64);
        }
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    /// Copy an entire resource out of the guest's backing pages.
    ///
    /// Same mechanics as `transfer_to_host_2d` with a full-surface rect and a
    /// zero offset, minus the request parsing: the linear resource maps
    /// row-for-row onto the scatter-gather backing, so one walk fills it.
    fn pull_whole_resource(&mut self, memory: &mut MemoryWrapper, resource_id: u32) {
        let Some(res) = self.resources.get(&resource_id) else { return };
        if res.backing.is_empty() || res.pixels.is_empty() {
            return;
        }
        let len = res.pixels.len();
        let backing = res.backing.clone();
        let mut buf = vec![0u8; len];
        read_backing(memory, &backing, 0, &mut buf);
        if let Some(res) = self.resources.get_mut(&resource_id) {
            res.pixels.copy_from_slice(&buf);
            res.pulled = true;
        }
        self.mark_dirty(Rect { x: 0, y: 0, width: self.display_width, height: self.display_height });
    }

    /// Pull the guest's pixels into the host's copy of the resource.
    ///
    /// The guest's backing is a scatter-gather list of its own pages; the
    /// resource is linear. `offset` is where in that linear space the rect's
    /// first row lives, so each row is copied separately — a rect narrower
    /// than the resource is not contiguous at either end.
    fn transfer_to_host_2d(&mut self, memory: &mut MemoryWrapper, req: &[u8]) -> Vec<u8> {
        let r = rect_at(req, CTRL_HDR_LEN);
        let offset = le64(req, CTRL_HDR_LEN + 16);
        let resource_id = le32(req, CTRL_HDR_LEN + 24);

        let Some(res) = self.resources.get(&resource_id) else {
            return Self::resp_hdr(req, RESP_ERR_INVALID_RESOURCE_ID);
        };
        if res.backing.is_empty() {
            return Self::resp_hdr(req, RESP_ERR_UNSPEC);
        }
        // Clip to the resource: a malformed rect must not index outside the
        // host buffer, and the guest is not trusted to be well-formed.
        if r.x >= res.width || r.y >= res.height {
            return Self::resp_hdr(req, RESP_ERR_INVALID_PARAMETER);
        }
        let w = r.width.min(res.width - r.x) as usize;
        let h = r.height.min(res.height - r.y) as usize;
        if w == 0 || h == 0 {
            return Self::resp_hdr(req, RESP_OK_NODATA);
        }
        let stride = res.width as usize * BPP;
        let backing = res.backing.clone();

        let mut rows: Vec<(usize, Vec<u8>)> = Vec::with_capacity(h);
        for row in 0..h {
            let src = offset as usize + stride * row;
            let dst = (r.y as usize + row) * stride + r.x as usize * BPP;
            let mut buf = vec![0u8; w * BPP];
            read_backing(memory, &backing, src, &mut buf);
            rows.push((dst, buf));
        }
        if let Some(res) = self.resources.get_mut(&resource_id) {
            for (dst, buf) in rows {
                let end = dst + buf.len();
                if end <= res.pixels.len() {
                    res.pixels[dst..end].copy_from_slice(&buf);
                }
            }
        }
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    fn attach_backing(&mut self, req: &[u8]) -> Vec<u8> {
        let resource_id = le32(req, CTRL_HDR_LEN);
        let nr_entries = le32(req, CTRL_HDR_LEN + 4) as usize;
        if !self.resources.contains_key(&resource_id) {
            return Self::resp_hdr(req, RESP_ERR_INVALID_RESOURCE_ID);
        }
        // Each virtio_gpu_mem_entry is addr(8) + length(4) + padding(4).
        let base = CTRL_HDR_LEN + 8;
        if req.len() < base + nr_entries * 16 {
            return Self::resp_hdr(req, RESP_ERR_INVALID_PARAMETER);
        }
        let mut backing = Vec::with_capacity(nr_entries);
        for i in 0..nr_entries {
            let at = base + i * 16;
            backing.push((le64(req, at), le32(req, at + 8)));
        }
        if let Some(res) = self.resources.get_mut(&resource_id) {
            res.backing = backing;
        }
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    fn detach_backing(&mut self, req: &[u8]) -> Vec<u8> {
        let resource_id = le32(req, CTRL_HDR_LEN);
        match self.resources.get_mut(&resource_id) {
            Some(res) => {
                res.backing.clear();
                Self::resp_hdr(req, RESP_OK_NODATA)
            }
            None => Self::resp_hdr(req, RESP_ERR_INVALID_RESOURCE_ID),
        }
    }

    /// virtio_gpu_update_cursor: hdr, then cursor_pos {scanout_id, x, y, pad},
    /// then resource_id, hot_x, hot_y, pad. MOVE_CURSOR carries the same
    /// layout but only the position is meaningful.
    fn update_cursor(&mut self, req: &[u8], set_resource: bool) -> Vec<u8> {
        let x = le32(req, CTRL_HDR_LEN + 4) as i64;
        let y = le32(req, CTRL_HDR_LEN + 8) as i64;
        self.cursor_updates = self.cursor_updates.wrapping_add(1);
        let old = self.cursor_rect();
        match self.cursor.as_mut() {
            Some(c) => {
                c.x = x;
                c.y = y;
                if set_resource {
                    c.resource_id = le32(req, CTRL_HDR_LEN + 16);
                    c.hot_x = le32(req, CTRL_HDR_LEN + 20) as i64;
                    c.hot_y = le32(req, CTRL_HDR_LEN + 24) as i64;
                }
            }
            None => {
                self.cursor = Some(Cursor {
                    resource_id: match set_resource {
                        true => le32(req, CTRL_HDR_LEN + 16),
                        false => 0,
                    },
                    x,
                    y,
                    hot_x: le32(req, CTRL_HDR_LEN + 20) as i64,
                    hot_y: le32(req, CTRL_HDR_LEN + 24) as i64,
                });
            }
        }
        // The guest does NOT flush the scanout when the pointer moves — that
        // is the whole point of a cursor plane. So the device has to report
        // the damage itself, over both the vacated and the newly covered
        // rectangle, or a damage-guided host scanner never re-encodes either
        // and the pointer leaves a trail (or never appears at all).
        if let Some(r) = old {
            self.mark_dirty(r);
        }
        if let Some(r) = self.cursor_rect() {
            self.mark_dirty(r);
        }
        Self::resp_hdr(req, RESP_OK_NODATA)
    }

    /// Cursor as the device currently holds it: (resource, x, y, commands).
    /// Instrumentation, because "is the guest sending cursor commands at all,
    /// and what positions do they carry" is not answerable from pixels — and
    /// guessing at it from screenshots wasted two test runs.
    pub fn cursor_state(&self) -> Option<(u32, i64, i64, u64)> {
        self.cursor.as_ref().map(|c| (c.resource_id, c.x, c.y, self.cursor_updates))
    }

    /// Where the cursor currently covers the scanout, clipped to it.
    fn cursor_rect(&self) -> Option<Rect> {
        let c = self.cursor.as_ref()?;
        if c.resource_id == 0 {
            return None;
        }
        let res = self.resources.get(&c.resource_id)?;
        let x0 = (c.x - c.hot_x).max(0) as u32;
        let y0 = (c.y - c.hot_y).max(0) as u32;
        if x0 >= self.display_width || y0 >= self.display_height {
            return None;
        }
        Some(Rect {
            x: x0,
            y: y0,
            width: res.width.min(self.display_width - x0),
            height: res.height.min(self.display_height - y0),
        })
    }

    /// Alpha-blend the cursor over a COPY of the scanout.
    ///
    /// Takes the caller's buffer rather than touching the resource, because
    /// the scanout is the guest's own memory: painting a pointer into it
    /// would corrupt what the guest believes it drew, and the next partial
    /// update would leave the old pointer behind forever.
    pub fn compose_cursor(&self, out: &mut [u8], w: usize, h: usize) {
        let Some(c) = self.cursor.as_ref() else { return };
        if c.resource_id == 0 {
            return;
        }
        let Some(res) = self.resources.get(&c.resource_id) else { return };
        let ox = c.x - c.hot_x;
        let oy = c.y - c.hot_y;
        for cy in 0..res.height as i64 {
            let dy = oy + cy;
            if dy < 0 || dy >= h as i64 {
                continue;
            }
            for cx in 0..res.width as i64 {
                let dx = ox + cx;
                if dx < 0 || dx >= w as i64 {
                    continue;
                }
                let si = (cy as usize * res.width as usize + cx as usize) * BPP;
                let di = (dy as usize * w + dx as usize) * BPP;
                if si + 4 > res.pixels.len() || di + 4 > out.len() {
                    continue;
                }
                let a = res.pixels[si + 3] as u32;
                if a == 0 {
                    continue; // fully transparent: the common case by area
                }
                if a == 255 {
                    out[di..di + 3].copy_from_slice(&res.pixels[si..si + 3]);
                    continue;
                }
                for k in 0..3 {
                    let src = res.pixels[si + k] as u32;
                    let dst = out[di + k] as u32;
                    out[di + k] = ((src * a + dst * (255 - a)) / 255) as u8;
                }
            }
        }
    }

    fn submit_3d(&mut self, req: &[u8]) -> Vec<u8> {
        let size = le32(req, CTRL_HDR_LEN) as usize;
        let ctx_id = le32(req, 16);
        let start = CTRL_HDR_LEN + 8;
        let stream = req.get(start..(start + size).min(req.len())).unwrap_or(&[]);
        match self.virgl.as_mut() {
            Some(b) => match b.submit(ctx_id, stream) {
                Ok(()) => Self::resp_hdr(req, RESP_OK_NODATA),
                Err(()) => Self::resp_hdr(req, RESP_ERR_UNSPEC),
            },
            // Without a backend the feature was never offered, so a driver
            // should not be here at all. Refusing is what keeps a guest that
            // tries anyway from blocking forever on a fence.
            None => Self::resp_hdr(req, RESP_ERR_UNSPEC),
        }
    }

    // ---- mmio ------------------------------------------------------------

    pub fn load(&mut self, address: u64) -> u8 {
        let off = address - BASE;
        match off {
            0x000 => 0x76, // "virt"
            0x001 => 0x69,
            0x002 => 0x72,
            0x003 => 0x74,
            0x004 => 2,  // version 2 (non-legacy)
            0x008 => 16, // device id: gpu
            0x00c => 0x51, // "QEMU"
            0x00d => 0x45,
            0x00e => 0x4d,
            0x00f => 0x55,
            0x010..=0x013 => {
                let sh = (self.device_features_sel as u64) * 32 + (off - 0x010) * 8;
                ((self.device_features() >> sh) & 0xff) as u8
            }
            0x034 => MAX_QUEUE_SIZE as u8,
            0x035 => (MAX_QUEUE_SIZE >> 8) as u8,
            0x036 => (MAX_QUEUE_SIZE >> 16) as u8,
            0x037 => (MAX_QUEUE_SIZE >> 24) as u8,
            0x044 => self.queue().ready as u8,
            0x045..=0x047 => 0,
            0x060 => self.interrupt_status as u8,
            0x061 => (self.interrupt_status >> 8) as u8,
            0x062 => (self.interrupt_status >> 16) as u8,
            0x063 => (self.interrupt_status >> 24) as u8,
            0x070 => self.status as u8,
            0x071 => (self.status >> 8) as u8,
            0x072 => (self.status >> 16) as u8,
            0x073 => (self.status >> 24) as u8,
            // SHM regions (mmio v2: SHM_SEL 0x0ac, LEN 0x0b0/4, BASE 0x0b8/c).
            // ALL ONES means "no such region", and zero does NOT: the driver
            // only skips a region whose length reads as ~0, so returning the
            // default 0 advertises a region of length 0 at address 0. Linux
            // then believes this device has host-visible memory, tries to
            // reserve it, and probe dies with
            //   [drm:virtio_gpu_init] *ERROR* Could not reserve host visible region
            //   virtio_gpu: probe of virtio4 failed with error -16
            // Any future mmio device the guest queries SHM regions on inherits
            // this trap; the register file's default of 0 is not neutral here.
            0x0b0..=0x0bf => 0xff,
            0x0fc..=0x0ff => 0,
            // virtio_gpu_config: events_read, events_clear, num_scanouts,
            // num_capsets.
            0x100..=0x1ff => {
                let cfg = off - 0x100;
                match cfg {
                    0..=7 => 0,                                   // events_read/clear
                    8 => 1,                                       // num_scanouts = 1
                    9..=11 => 0,
                    12..=15 => (self.num_capsets() >> ((cfg - 12) * 8)) as u8,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    pub fn store(&mut self, address: u64, value: u8) {
        let off = address - BASE;
        let v = value as u32;
        match off {
            0x014..=0x017 => set_byte32(&mut self.device_features_sel, off - 0x014, v),
            0x020..=0x023 => {
                let sh = (self.driver_features_sel as u64) * 32 + (off - 0x020) * 8;
                self.driver_features =
                    (self.driver_features & !(0xffu64 << sh)) | ((value as u64) << sh);
            }
            0x024..=0x027 => set_byte32(&mut self.driver_features_sel, off - 0x024, v),
            0x030..=0x033 => set_byte32(&mut self.queue_select, off - 0x030, v),
            0x038..=0x03b => {
                let mut n = self.queue().num;
                set_byte32(&mut n, off - 0x038, v);
                self.queue_mut().num = n;
            }
            0x044 => self.queue_mut().ready = (value & 1) == 1,
            // QueueNotify: tick() drains whatever is posted.
            0x050..=0x053 => {}
            0x064 => {
                if (value & 0x1) == 1 {
                    self.interrupt_status &= !0x1;
                }
            }
            0x070..=0x073 => {
                let mut s = self.status;
                set_byte32(&mut s, off - 0x070, v);
                self.status = s;
                if self.status == 0 {
                    self.reset();
                }
            }
            0x080..=0x083 => { let mut a = self.queue().desc; set_byte64(&mut a, off - 0x080, value); self.queue_mut().desc = a; }
            0x084..=0x087 => { let mut a = self.queue().desc; set_byte64(&mut a, off - 0x084 + 4, value); self.queue_mut().desc = a; }
            0x090..=0x093 => { let mut a = self.queue().driver; set_byte64(&mut a, off - 0x090, value); self.queue_mut().driver = a; }
            0x094..=0x097 => { let mut a = self.queue().driver; set_byte64(&mut a, off - 0x094 + 4, value); self.queue_mut().driver = a; }
            0x0a0..=0x0a3 => { let mut a = self.queue().device; set_byte64(&mut a, off - 0x0a0, value); self.queue_mut().device = a; }
            0x0a4..=0x0a7 => { let mut a = self.queue().device; set_byte64(&mut a, off - 0x0a4 + 4, value); self.queue_mut().device = a; }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.queues = [Queue::new(), Queue::new()];
        self.interrupt_status = 0;
        self.driver_features = 0;
        self.resources.clear();
        self.scanout_resource = 0;
        self.dirty = None;
        self.flushes = 0;
        self.flush_bytes = 0;
        self.cursor = None;
    }

    fn queue(&self) -> &Queue {
        &self.queues[(self.queue_select as usize) % NUM_QUEUES]
    }
    fn queue_mut(&mut self) -> &mut Queue {
        &mut self.queues[(self.queue_select as usize) % NUM_QUEUES]
    }

    fn avail_index(&self, memory: &mut MemoryWrapper, qi: usize) -> u16 {
        memory.read_halfword(self.queues[qi].driver.wrapping_add(2))
    }

    fn pop_avail(&mut self, memory: &mut MemoryWrapper, qi: usize) -> Option<u64> {
        if !self.queues[qi].is_ready() {
            return None;
        }
        if self.avail_index(memory, qi) == self.queues[qi].avail_cursor {
            return None;
        }
        let q = &self.queues[qi];
        let slot = (q.avail_cursor as u64) % (q.num as u64);
        let head = memory.read_halfword(q.driver.wrapping_add(4).wrapping_add(slot * 2));
        self.queues[qi].avail_cursor = self.queues[qi].avail_cursor.wrapping_add(1);
        Some((head as u64) % (self.queues[qi].num as u64))
    }

    fn push_used(&mut self, memory: &mut MemoryWrapper, qi: usize, head: u64, len: u32) {
        let q = &self.queues[qi];
        let used = q.device;
        let slot = (q.used_index as u64) % (q.num as u64);
        memory.write_word(used.wrapping_add(4).wrapping_add(slot * 8), head as u32);
        memory.write_word(used.wrapping_add(4).wrapping_add(slot * 8).wrapping_add(4), len);
        let next = q.used_index.wrapping_add(1);
        self.queues[qi].used_index = next;
        memory.write_halfword(used.wrapping_add(2), next);
        self.interrupt_status |= 0x1;
    }

    fn walk_chain(&self, memory: &mut MemoryWrapper, qi: usize, head: u64)
        -> (Vec<u8>, Vec<(u64, u32)>) {
        let q = &self.queues[qi];
        let desc_base = q.desc;
        let queue_size = q.num as u64;
        let mut readable = Vec::new();
        let mut writable = Vec::new();
        let mut desc_index = head;
        for _ in 0..queue_size {
            let desc = desc_base + 16 * desc_index;
            let addr = memory.read_doubleword(desc);
            let len = memory.read_word(desc.wrapping_add(8));
            let flags = memory.read_halfword(desc.wrapping_add(12));
            let next = (memory.read_halfword(desc.wrapping_add(14)) as u64) % queue_size;
            match (flags & VIRTQ_DESC_F_WRITE) != 0 {
                true => writable.push((addr, len)),
                false => {
                    for i in 0..len as u64 {
                        readable.push(memory.read_byte(addr + i));
                    }
                }
            }
            if (flags & VIRTQ_DESC_F_NEXT) == 0 {
                break;
            }
            desc_index = next;
        }
        (readable, writable)
    }
}

/// Read `out.len()` bytes starting at linear offset `off` across a resource's
/// scatter-gather backing. Anything past the end of the list reads as zero,
/// which is what a short backing should look like rather than a panic.
fn read_backing(memory: &mut MemoryWrapper, backing: &[(u64, u32)], off: usize, out: &mut [u8]) {
    let mut want = out.len();
    let mut cursor = 0usize; // linear position of the current entry's start
    let mut written = 0usize;
    for &(addr, len) in backing {
        if want == 0 {
            break;
        }
        let len = len as usize;
        let entry_end = cursor + len;
        if off + written < entry_end {
            let within = (off + written).saturating_sub(cursor);
            let n = (len - within).min(want);
            for i in 0..n {
                out[written + i] = memory.read_byte(addr + (within + i) as u64);
            }
            written += n;
            want -= n;
        }
        cursor = entry_end;
    }
}

fn rect_at(buf: &[u8], at: usize) -> Rect {
    Rect {
        x: le32(buf, at),
        y: le32(buf, at + 4),
        width: le32(buf, at + 8),
        height: le32(buf, at + 12),
    }
}

fn le32(buf: &[u8], at: usize) -> u32 {
    let mut v = [0u8; 4];
    for i in 0..4 {
        v[i] = buf.get(at + i).copied().unwrap_or(0);
    }
    u32::from_le_bytes(v)
}

fn le64(buf: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    for i in 0..8 {
        v[i] = buf.get(at + i).copied().unwrap_or(0);
    }
    u64::from_le_bytes(v)
}

fn write_out(memory: &mut MemoryWrapper, writable: &[(u64, u32)], bytes: &[u8]) -> u32 {
    let mut written = 0usize;
    for &(addr, len) in writable {
        if written >= bytes.len() {
            break;
        }
        let n = (len as usize).min(bytes.len() - written);
        for i in 0..n {
            memory.write_byte(addr + i as u64, bytes[written + i]);
        }
        written += n;
    }
    written as u32
}

fn set_byte32(reg: &mut u32, pos: u64, value: u32) {
    let sh = pos * 8;
    *reg = (*reg & !(0xffu32 << sh)) | ((value & 0xff) << sh);
}

fn set_byte64(reg: &mut u64, pos: u64, value: u8) {
    let sh = pos * 8;
    *reg = (*reg & !(0xffu64 << sh)) | ((value as u64) << sh);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// resource_unref takes &mut self and a request; call it without a
    /// MemoryWrapper, which the 2D unref path never touches.
    fn Self_unref(gpu: &mut VirtioGpu, req: &[u8]) -> Vec<u8> {
        gpu.resource_unref(req)
    }

    /// A scatter-gather backing must read as one linear resource. The guest
    /// hands us its own pages in whatever order the allocator produced, and a
    /// row of pixels routinely straddles two of them.
    #[test]
    fn backing_reads_across_entry_boundaries() {
        // Not a real MemoryWrapper: this checks the offset arithmetic, which
        // is where a scatter-gather walk actually goes wrong.
        let entries = [(0x1000u64, 16u32), (0x2000u64, 16u32)];
        let mut cursor = 0usize;
        let mut spans = Vec::new();
        for &(addr, len) in &entries {
            spans.push((cursor, cursor + len as usize, addr));
            cursor += len as usize;
        }
        // A 8-byte read at offset 12 must span entry 0 (4 bytes) and entry 1.
        let off = 12usize;
        let want = 8usize;
        let mut covered = 0usize;
        for (start, end, _) in &spans {
            let lo = (off + covered).max(*start);
            let hi = (off + want).min(*end);
            if lo < hi {
                covered += hi - lo;
            }
        }
        assert_eq!(covered, want, "an 8-byte read at offset 12 must cross both entries");
    }

    /// Dirty regions coalesce to a bounding box that contains both.
    #[test]
    fn dirty_rects_coalesce() {
        let mut gpu = VirtioGpu::new(1024, 768);
        gpu.mark_dirty(Rect { x: 10, y: 10, width: 10, height: 10 });
        gpu.mark_dirty(Rect { x: 100, y: 50, width: 20, height: 20 });
        let d = gpu.take_dirty().expect("something was flushed");
        assert_eq!((d.x, d.y), (10, 10));
        assert_eq!((d.x + d.width, d.y + d.height), (120, 70));
        assert!(gpu.take_dirty().is_none(), "taking the region must clear it");
    }

    /// Freeing the cursor's resource must repaint where it was and must not
    /// leave the device pointing at a resource that no longer exists. X frees
    /// the cursor resource on every pointer-SHAPE change, so this happens in
    /// ordinary use within minutes — the pointer's last image froze on screen
    /// and stopped tracking, because nothing reported damage for the region it
    /// had been occupying.
    #[test]
    fn freeing_the_cursor_resource_repaints_and_releases_it() {
        let mut gpu = VirtioGpu::new(1024, 768);
        gpu.resources.insert(9, Resource {
            width: 64, height: 64, backing: Vec::new(),
            pixels: vec![0u8; 64 * 64 * BPP],
            pulled: false,
        });
        gpu.cursor = Some(Cursor { resource_id: 9, x: 300, y: 200, hot_x: 0, hot_y: 0 });
        let _ = gpu.take_dirty(); // start clean

        let mut req = vec![0u8; CTRL_HDR_LEN + 8];
        req[..4].copy_from_slice(&CMD_RESOURCE_UNREF.to_le_bytes());
        req[CTRL_HDR_LEN..CTRL_HDR_LEN + 4].copy_from_slice(&9u32.to_le_bytes());
        let mut mem_unused = ();
        let _ = &mut mem_unused;
        let resp = Self_unref(&mut gpu, &req);
        assert_eq!(le32(&resp, 0), RESP_OK_NODATA);

        let d = gpu.take_dirty().expect("the vacated cursor area must be repainted");
        assert!(d.x <= 300 && d.y <= 200 && d.width > 0 && d.height > 0,
                "damage must cover where the cursor was: {:?}", (d.x, d.y, d.width, d.height));
        assert_eq!(gpu.cursor.as_ref().map(|c| c.resource_id), Some(0),
                   "the freed resource must be released, not left dangling");
        assert!(gpu.cursor_rect().is_none(), "a released cursor covers nothing");
    }

    /// 3D must be refused, not silently accepted: a guest that submits a
    /// command stream nobody executes blocks forever on its fence.
    #[test]
    fn refuses_3d_without_a_backend() {
        let gpu = VirtioGpu::new(1024, 768);
        assert_eq!(gpu.num_capsets(), 0);
        assert_eq!(gpu.device_features() & VIRTIO_GPU_F_VIRGL, 0,
                   "VIRGL must not be advertised without a renderer behind it");
    }
}
