//
// RISC Box X11 video backend — the X protocol, spoken directly.
//
// DOOM on this machine is an X client on the shipped Alpine desktop, and the
// pixel path it normally takes costs about twenty emulated instructions per
// presented pixel: SDL converts 8-bit to ARGB, blits it through a generic
// scaler into a differently-formatted surface, and hands the result to Xlib.
// At 640x480 that is 6M instructions a frame — four times what DOOM's own
// renderer costs — and none of it is work the picture needs.
//
// This backend keeps the window, the desktop and the screen exactly as they
// are and removes the waste instead:
//
//   * no Xlib. The X11 wire protocol for what a game needs — connect, create
//     a window, map it, PutImage, read keys — is a few hundred lines, and
//     going straight to the socket means no library-side buffering, no format
//     negotiation per frame, and a static binary that does not care that the
//     guest is musl and this toolchain is glibc.
//   * the palette lookup is FUSED INTO THE SCALE. One 256-entry table turns
//     DOOM's byte into the server's pixel, and an integer scale writes it as
//     64-bit stores, two pixels at a time. About two instructions per pixel.
//   * one copy to the server, in row-chunks that fit the maximum request
//     length, straight out of the buffer the scale wrote.
//
// Copyright (C) 1993-1996 Id Software, Inc.; GPLv2, like the rest of this tree.
//

#include "config.h"
#include "v_video.h"
#include "m_argv.h"
#include "d_event.h"
#include "d_main.h"
#include "i_system.h"
#include "i_video.h"
#include "z_zone.h"
#include "tables.h"
#include "doomkeys.h"

#include <stdbool.h>
#include <stddef.h>   /* offsetof: not pulled in transitively by newer libc headers */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/time.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <linux/fb.h>
#include <stddef.h>

// globals the rest of the tree expects from a video/input backend
byte *I_VideoBuffer = NULL;
boolean screenvisible;
boolean screensaver_mode = false;
int usegamma = 0;
int usemouse = 0;
int vanilla_keyboard_mapping = 1;
float mouse_acceleration = 2.0;
int mouse_threshold = 10;
int X_width, X_height;

static int xfd = -1;
static uint32_t id_base, id_mask, next_id;
static uint32_t root_win, root_visual, win, gc;
static uint32_t max_request;      // in 4-byte units
static int root_w, root_h, root_depth;
static int win_w, win_h, scale;
static uint32_t *img;             // the scaled frame, exactly as the server wants it
static uint32_t palette[256];

// MIT-SHM. Without it every frame is copied twice more than it needs to be:
// once into the socket by us and once out of it by the server. With it, the
// scale writes straight into memory the server can already see, and the whole
// frame becomes a 40-byte request.
#define NBUF 2
static int shm_op;                // extension's major opcode, 0 = unavailable
static int shm_ev;                // first event code (completion)
static uint32_t shm_seg[NBUF];
static int shm_id[NBUF];
static uint32_t *shm_buf[NBUF];
static int cur_buf, pending;

// Direct-to-framebuffer overlay (opt-in, -overlay; see I_InitGraphics).
//
// The idea: with MIT-SHM the frame is written once by us and copied once by
// the server, and the pixels are already in the same DRAM the framebuffer
// lives in, so the scale could write THERE and the copy would stop happening.
// New hosts snapshot each completed frame and composite it over the desktop
// natively. This avoids the X server's shadow/scanout copies in the guest,
// while the explicit ownership protocol supports fullscreen and static menus.
//
// It is only safe while the window is fully visible and where it is, so the
// window's origin is tracked with TranslateCoordinates and its visibility with
// VisibilityNotify; anything else (obscured, moved and not yet re-resolved, no
// /dev/fb0) falls back to the ShmPutImage path, which is always correct.
static uint8_t *fbmem;
static size_t fbmem_len;
static int fb_stride, fb_w_px, fb_h_px;
static int org_x = -1, org_y = -1;    // window origin in root coordinates
static int visible;                   // VisibilityUnobscured
static int overlay_frames, x_frames;  // how each frame got to the screen
static volatile uint32_t *overlay_ctl;

static void overlay_publish(int active)
{
    if (!overlay_ctl)
        return;
    if (active) {
        overlay_ctl[2] = org_x;
        overlay_ctl[3] = org_y;
        overlay_ctl[4] = win_w;
        overlay_ctl[5] = win_h;
    }
    __sync_synchronize();
    *(volatile uint8_t *)((volatile uint8_t *)overlay_ctl + 4) = active;
}

static void overlay_hide(void) { overlay_publish(0); }

static int frames;
static struct timeval fps_t0;
static int fps_report = 1;

void I_GetEvent_blocking(void);
static int have_prev;   // defined with `prev` below

static uint32_t new_id(void)
{
    return id_base | ((next_id++) & id_mask);
}

// ------------------------------------------------------------------ wire ---

static int wr(const void *p, size_t n)
{
    const char *b = p;
    while (n) {
        ssize_t k = write(xfd, b, n);
        if (k < 0) {
            if (errno == EINTR)
                continue;
            return -1;
        }
        b += k;
        n -= k;
    }
    return 0;
}

static int rd(void *p, size_t n)
{
    char *b = p;
    while (n) {
        ssize_t k = read(xfd, b, n);
        if (k <= 0) {
            if (k < 0 && errno == EINTR)
                continue;
            return -1;
        }
        b += k;
        n -= k;
    }
    return 0;
}

// The display's socket. Xorg listens on the filesystem path in this image
// (/tmp/.X11-unix is a tmpfs the appliance init creates); the abstract-socket
// form is tried first because a stock Xorg binds both.
static int x_connect(void)
{
    const char *d = getenv("DISPLAY");
    int num = 0;
    struct sockaddr_un sa;
    socklen_t len;

    if (d && (d = strchr(d, ':')))
        num = atoi(d + 1);

    for (int abstract = 1; abstract >= 0; abstract--) {
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0)
            return -1;
        memset(&sa, 0, sizeof(sa));
        sa.sun_family = AF_UNIX;
        if (abstract) {
            // leading NUL = abstract namespace
            sa.sun_path[0] = '\0';
            snprintf(sa.sun_path + 1, sizeof(sa.sun_path) - 1, "/tmp/.X11-unix/X%d", num);
            len = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(sa.sun_path + 1);
        } else {
            snprintf(sa.sun_path, sizeof(sa.sun_path), "/tmp/.X11-unix/X%d", num);
            len = sizeof(sa);
        }
        if (connect(fd, (struct sockaddr *)&sa, len) == 0)
            return fd;
        close(fd);
    }
    return -1;
}

// Connection setup. No authorization data is sent: this image's Xorg is
// started without -auth, so local clients connect unauthenticated (which is
// also why xterm and fluxbox come up with no XAUTHORITY set).
static int x_setup(void)
{
    struct {
        uint8_t order, pad;
        uint16_t major, minor, nlen, dlen, pad2;
    } req = { 'l', 0, 11, 0, 0, 0, 0 };
    uint8_t hdr[8];
    uint8_t *body;
    uint32_t blen;
    uint8_t *p;
    int nformats, nscreens, ndepths;
    uint16_t vendor_len;

    if (wr(&req, sizeof(req)) < 0 || rd(hdr, 8) < 0)
        return -1;
    if (hdr[0] != 1) {
        printf("I_InitGraphics: X refused the connection (code %d)\n", hdr[0]);
        return -1;
    }
    blen = ((uint32_t)hdr[6] | ((uint32_t)hdr[7] << 8)) * 4;
    body = malloc(blen);
    if (!body || rd(body, blen) < 0)
        return -1;

    id_base = *(uint32_t *)(body + 4);
    id_mask = *(uint32_t *)(body + 8);
    vendor_len = *(uint16_t *)(body + 16);
    max_request = *(uint16_t *)(body + 18);
    nscreens = body[20];
    nformats = body[21];

    p = body + 32 + ((vendor_len + 3) & ~3) + 8 * nformats;
    if (nscreens < 1)
        return -1;
    root_win = *(uint32_t *)(p + 0);
    root_w = *(uint16_t *)(p + 20);
    root_h = *(uint16_t *)(p + 22);
    root_visual = *(uint32_t *)(p + 32);
    root_depth = p[38];
    ndepths = p[39];
    (void)ndepths;
    free(body);
    next_id = 0;
    return 0;
}

static void wm_delete_setup(void);
static void wm_fullscreen_setup(void);

static void x_create_window(void)
{
    struct {
        uint8_t op, depth;
        uint16_t len;
        uint32_t wid, parent;
        int16_t x, y;
        uint16_t w, h, border, class;
        uint32_t visual, mask, bg, events;
    } cw;
    struct {
        uint8_t op, pad;
        uint16_t len;
        uint32_t win;
    } mw;
    struct {
        uint8_t op, pad;
        uint16_t len;
        uint32_t cid, drawable, mask;
    } cg;

    win = new_id();
    cw.op = 1;
    cw.depth = 0;                 // CopyFromParent
    cw.len = sizeof(cw) / 4;
    cw.wid = win;
    cw.parent = root_win;
    cw.x = (root_w - win_w) / 2;
    cw.y = (root_h - win_h) / 2;
    cw.w = win_w;
    cw.h = win_h;
    cw.border = 0;
    cw.class = 1;                 // InputOutput
    cw.visual = 0;                // CopyFromParent
    cw.mask = 0x00000002 | 0x00000800;   // CWBackPixel | CWEventMask
    cw.bg = 0;
    // KeyPress | KeyRelease | Exposure | StructureNotify
    // KeyPress | KeyRelease | Exposure | VisibilityChange | StructureNotify
    cw.events = 0x00000001 | 0x00000002 | 0x00008000 | 0x00010000 | 0x00020000;
    wr(&cw, sizeof(cw));

    // Register for the WM's close button BEFORE the map: the window manager
    // reads WM_PROTOCOLS when it starts managing the window, and a property
    // that arrives after MapWindow was never seen — the close button then
    // falls back to XKillClient.
    wm_delete_setup();
    if (win_w == root_w && win_h == root_h)
        wm_fullscreen_setup();

    mw.op = 8;
    mw.pad = 0;
    mw.len = 2;
    mw.win = win;
    wr(&mw, sizeof(mw));

    gc = new_id();
    cg.op = 55;
    cg.pad = 0;
    cg.len = 4;
    cg.cid = gc;
    cg.drawable = win;
    cg.mask = 0;
    wr(&cg, sizeof(cg));
}

// ------------------------------------------------- window-manager close ---
// The WM's close button is a ClientMessage carrying WM_DELETE_WINDOW — but
// only to clients that registered for it. Without this the button falls back
// to XKillClient: the frame vanishes while the game keeps running headless,
// which on the desktop read as "closing DOOM doesn't close DOOM".
static uint32_t wm_protocols, wm_delete;

static uint32_t intern_atom(const char *name)
{
    size_t n = strlen(name);
    size_t pad = (4 - (n & 3)) & 3;
    static const uint8_t zeros[4];
    struct {
        uint8_t op, only;
        uint16_t len;
        uint16_t nlen, pad2;
    } req;
    uint8_t reply[32];

    req.op = 16;                  // InternAtom
    req.only = 0;
    req.len = 2 + (n + pad) / 4;
    req.nlen = n;
    req.pad2 = 0;
    if (wr(&req, sizeof(req)) < 0 || wr((void *)name, n) < 0)
        return 0;
    if (pad && wr((void *)zeros, pad) < 0)
        return 0;
    // Synchronous: the event pump is not running yet, and nothing else with a
    // reply is outstanding. Anything that is not a reply (an early MapNotify,
    // an error) is a 32-byte unit, already consumed by the read.
    for (;;) {
        if (rd(reply, 32) < 0)
            return 0;
        if (reply[0] == 1)
            return *(uint32_t *)(reply + 8);
    }
}

// A screen-sized client needs the WM's fullscreen state before MapWindow.
// Otherwise fluxbox adds decorations and moves the client upward to fit,
// clipping the game even though its framebuffer has the right dimensions.
static void wm_fullscreen_setup(void)
{
    uint32_t state = intern_atom("_NET_WM_STATE");
    uint32_t fullscreen = intern_atom("_NET_WM_STATE_FULLSCREEN");
    if (!state || !fullscreen)
        return;
    struct {
        uint8_t op, mode;
        uint16_t len;
        uint32_t win, prop, type;
        uint8_t fmt, pad[3];
        uint32_t n, atom;
    } cp = { 18, 0, 7, win, state, 4, 32, {0, 0, 0}, 1, fullscreen };
    wr(&cp, sizeof(cp));
    printf("I_InitGraphics: requested fullscreen %dx%d\n", win_w, win_h);
}

static void wm_delete_setup(void)
{
    struct {
        uint8_t op, mode;
        uint16_t len;
        uint32_t win, prop, type;
        uint8_t fmt, pad[3];
        uint32_t n;
        uint32_t atom;
    } cp;

    // WM_HINTS first: without an input hint the WM has no license to give
    // this window keyboard focus — fluxbox never focuses it, keys land in
    // whatever was focused before, and the game looks deaf: the menu cannot
    // open, so its QUIT is unreachable. XA_WM_HINTS is predefined atom 35.
    struct {
        uint8_t op, mode;
        uint16_t len;
        uint32_t win, prop, type;
        uint8_t fmt, pad[3];
        uint32_t n;
        uint32_t hints[9];
    } wh;
    wh.op = 18;                   // ChangeProperty
    wh.mode = 0;
    wh.len = sizeof(wh) / 4;
    wh.win = win;
    wh.prop = 35;                 // XA_WM_HINTS
    wh.type = 35;
    wh.fmt = 32;
    memset(wh.pad, 0, sizeof(wh.pad));
    wh.n = 9;
    memset(wh.hints, 0, sizeof(wh.hints));
    wh.hints[0] = 1 | 2;          // InputHint | StateHint
    wh.hints[1] = 1;              // input = True
    wh.hints[2] = 1;              // NormalState
    wr(&wh, sizeof(wh));

    // A name, so the taskbar stops saying "Unnamed". XA_WM_NAME 39, STRING 31.
    static const char name[] = "FreeDoom";
    struct {
        uint8_t op, mode;
        uint16_t len;
        uint32_t win, prop, type;
        uint8_t fmt, pad[3];
        uint32_t n;
        char text[8];
    } wn;
    wn.op = 18;
    wn.mode = 0;
    wn.len = sizeof(wn) / 4;
    wn.win = win;
    wn.prop = 39;                 // XA_WM_NAME
    wn.type = 31;                 // XA_STRING
    wn.fmt = 8;
    memset(wn.pad, 0, sizeof(wn.pad));
    wn.n = 8;
    memcpy(wn.text, name, 8);
    wr(&wn, sizeof(wn));

    wm_protocols = intern_atom("WM_PROTOCOLS");
    wm_delete = intern_atom("WM_DELETE_WINDOW");
    if (!wm_protocols || !wm_delete)
        return;
    cp.op = 18;                   // ChangeProperty
    cp.mode = 0;                  // Replace
    cp.len = sizeof(cp) / 4;
    cp.win = win;
    cp.prop = wm_protocols;
    cp.type = 4;                  // ATOM
    cp.fmt = 32;
    memset(cp.pad, 0, sizeof(cp.pad));
    cp.n = 1;
    cp.atom = wm_delete;
    wr(&cp, sizeof(cp));

    // Read it back: a silent X error would otherwise leave the protocol
    // unregistered with nothing in the log to say so.
    struct {
        uint8_t op, del;
        uint16_t len;
        uint32_t win, prop, type, off, count;
    } gp;
    uint8_t reply[32];
    gp.op = 20;                   // GetProperty
    gp.del = 0;
    gp.len = sizeof(gp) / 4;
    gp.win = win;
    gp.prop = wm_protocols;
    gp.type = 0;                  // AnyPropertyType
    gp.off = 0;
    gp.count = 8;
    wr(&gp, sizeof(gp));
    for (;;) {
        if (rd(reply, 32) < 0)
            return;
        if (reply[0] == 0) {
            printf("wm_delete_setup: X error code %d on seq %u\n",
                   reply[1], *(uint16_t *)(reply + 2));
            continue;
        }
        if (reply[0] != 1)
            continue;
        uint32_t vlen = *(uint32_t *)(reply + 4) * 4;
        uint32_t rtype = *(uint32_t *)(reply + 8);
        uint32_t nitems = *(uint32_t *)(reply + 16);
        uint8_t buf[64];
        uint32_t got = vlen > sizeof(buf) ? sizeof(buf) : vlen;
        if (got && rd(buf, got) < 0)
            return;
        printf("wm_delete_setup: readback type=%u fmt=%d items=%u first=%u\n",
               rtype, reply[1], nitems, nitems ? *(uint32_t *)buf : 0);
        break;
    }
}

// Map the framebuffer for the overlay path. Failure is not fatal: without it
// every frame simply goes through the server.
static void fb_open(void)
{
    struct fb_var_screeninfo vinfo;
    struct fb_fix_screeninfo finfo;
    int fd = open("/dev/fb0", O_RDWR);

    if (fd < 0)
        return;
    if (ioctl(fd, FBIOGET_VSCREENINFO, &vinfo) < 0 ||
        ioctl(fd, FBIOGET_FSCREENINFO, &finfo) < 0 ||
        vinfo.bits_per_pixel != 32) {
        close(fd);
        return;
    }
    fb_stride = finfo.line_length;
    fb_w_px = vinfo.xres;
    fb_h_px = vinfo.yres;
    fbmem_len = (size_t)fb_stride * fb_h_px;
    fbmem = mmap(NULL, fbmem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (fbmem == MAP_FAILED)
        fbmem = NULL;
    if (fbmem) {
        // New hosts accept explicit window ownership. Older hosts return
        // zero for unbacked device space and retain the legacy overlay.
        fd = open("/dev/mem", O_RDWR | O_SYNC);
        if (fd >= 0) {
            void *ctl = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0x10007000);
            close(fd);
            if (ctl != MAP_FAILED) {
                if (*(volatile uint32_t *)ctl == 0x4f584252u) {
                    overlay_ctl = ctl;
                    overlay_hide();
                    atexit(overlay_hide);
                    printf("I_InitGraphics: explicit host overlay ownership available\n");
                } else {
                    munmap(ctl, 4096);
                }
            }
        }
    }
}

// Where is this window on the root, right now? (The window manager reparents
// and decorates, so the answer is not the position we asked for.) The request
// is fire-and-forget: the reply is consumed by x_pump, the client's single
// reply/event reader — a second reader here raced it for the reply and could
// block forever on a reply the pump had already drained. Until the answer
// arrives org stays -1 and frames go through the server, which is always
// correct.
static int tc_pending;
static int tc_discard;   // in-flight replies made stale by a window move

static void x_resolve_origin(void)
{
    struct {
        uint8_t op, pad;
        uint16_t len;
        uint32_t src, dst;
        int16_t x, y;
    } tc = { 40, 0, 4, 0, 0, 0, 0 };

    if (tc_pending)
        return;
    tc.src = win;
    tc.dst = root_win;
    org_x = org_y = -1;
    if (wr(&tc, sizeof(tc)) < 0)
        return;
    tc_pending = 1;
}

// Ask for MIT-SHM and set up the shared frames. Returns 0 if anything is
// missing (no extension, no SysV IPC in the guest kernel, no room) — the
// PutImage path then carries on working, just at three copies a frame.
static int shm_init(size_t bytes)
{
    static const char name[] = "MIT-SHM";
    struct {
        uint8_t op, pad;
        uint16_t len, nlen, pad2;
        char n[8];               // 7 + pad to 4
    } qe = { 98, 0, 2 + sizeof(qe.n) / 4, sizeof(name) - 1, 0, "MIT-SHM" };
    uint8_t reply[32];
    int i;

    if (wr(&qe, sizeof(qe)) < 0 || rd(reply, sizeof(reply)) < 0)
        return 0;
    if (reply[0] != 1 || !reply[8])
        return 0;                // no such extension
    shm_op = reply[9];
    shm_ev = reply[10];

    for (i = 0; i < NBUF; i++) {
        struct {
            uint8_t op, minor;
            uint16_t len;
            uint32_t seg, id;
            uint8_t ro, pad[3];
        } at;

        shm_id[i] = shmget(IPC_PRIVATE, bytes, IPC_CREAT | 0600);
        if (shm_id[i] < 0)
            return 0;
        shm_buf[i] = shmat(shm_id[i], NULL, 0);
        if (shm_buf[i] == (void *)-1)
            return 0;
        shm_seg[i] = new_id();
        at.op = shm_op;
        at.minor = 1;            // ShmAttach
        at.len = 4;
        at.seg = shm_seg[i];
        at.id = shm_id[i];
        at.ro = 0;
        memset(at.pad, 0, sizeof(at.pad));
        if (wr(&at, sizeof(at)) < 0)
            return 0;
        // Marked for destruction now: the segment stays alive while it is
        // attached here and in the server, and disappears on exit however we
        // leave, rather than leaking into the guest's IPC namespace.
        shmctl(shm_id[i], IPC_RMID, NULL);
    }
    return 1;
}

// --------------------------------------------------------------- output ---

void I_InitGraphics(void)
{
    int i;

    setvbuf(stdout, NULL, _IONBF, 0);
    xfd = x_connect();
    if (xfd < 0) {
        printf("I_InitGraphics: cannot reach the X server on %s\n", getenv("DISPLAY"));
        exit(1);
    }
    if (x_setup() < 0)
        exit(1);

    i = M_CheckParmWithArgs("-scaling", 1);
    scale = i > 0 ? atoi(myargv[i + 1]) : 0;
    if (scale <= 0) {
        // biggest integer scale that fits the screen, leaving room for the
        // window manager's frame
        scale = (root_w - 8) / SCREENWIDTH;
        int sv = (root_h - 32) / SCREENHEIGHT;
        if (sv < scale)
            scale = sv;
    }
    if (scale < 1)
        scale = 1;
    win_w = SCREENWIDTH * scale;
    win_h = SCREENHEIGHT * scale;

    printf("I_InitGraphics: X %dx%d depth %d, window %dx%d (scale %d), max request %u KiB\n",
           root_w, root_h, root_depth, win_w, win_h, scale, (max_request * 4) / 1024);

    if (shm_init((size_t)win_w * win_h * 4)) {
        img = shm_buf[0];
        printf("I_InitGraphics: MIT-SHM opcode %d, %d shared frames\n", shm_op, NBUF);
    } else {
        shm_op = 0;
        img = malloc((size_t)win_w * win_h * 4);
        if (!img) {
            printf("I_InitGraphics: out of memory for a %dx%d frame\n", win_w, win_h);
            exit(1);
        }
        printf("I_InitGraphics: no MIT-SHM; falling back to PutImage\n");
    }
    memset(img, 0, (size_t)win_w * win_h * 4);

    x_create_window();
    printf("I_InitGraphics: WM_DELETE_WINDOW %s (atoms %u/%u)\n",
           wm_protocols && wm_delete ? "registered" : "UNAVAILABLE",
           wm_protocols, wm_delete);

    I_VideoBuffer = (byte *)Z_Malloc(SCREENWIDTH * SCREENHEIGHT, PU_STATIC, NULL);
    screenvisible = true;
    X_width = win_w;
    X_height = win_h;

    // Opt-IN, because it was measured and it did not pay: same binary, same
    // boot, same scenes, DOOM's own counter gave 24-40 fps with the overlay
    // against 35-40 with the server doing the copy. The server's blit out of
    // shared memory is simply not the expensive part it looked like, and an
    // overlay owes correctness work the copy gets for free -- it paints
    // outside the window whenever the window has moved and the origin has not
    // been re-resolved, and only the window manager knows when that is. Kept
    // behind a flag because the measurement, not the idea, is what settled it.
    if (M_CheckParm("-overlay")) {
        fb_open();
        if (fbmem) {
            x_resolve_origin();
            printf("I_InitGraphics: overlay on /dev/fb0 %dx%d stride %d, window at %d,%d\n",
                   fb_w_px, fb_h_px, fb_stride, org_x, org_y);
        } else {
            printf("I_InitGraphics: no /dev/fb0 overlay; every frame goes through X\n");
        }
    }

    if (M_CheckParm("-nofps"))
        fps_report = 0;
    gettimeofday(&fps_t0, NULL);
}

void I_ShutdownGraphics(void)
{
    overlay_hide();
    if (xfd >= 0)
        close(xfd);
    xfd = -1;
    free(img);
    img = NULL;
}

void I_StartFrame(void) {}
void I_UpdateNoBlit(void) {}

// One PutImage per chunk of rows that fits the server's maximum request.
static void put_rows(int y0, int rows)
{
    struct {
        uint8_t op, format;
        uint16_t len;
        uint32_t drawable, gc;
        uint16_t w, h;
        int16_t x, y;
        uint8_t left_pad, depth;
        uint16_t pad;
    } pi;
    size_t bytes = (size_t)win_w * rows * 4;

    pi.op = 72;
    pi.format = 2;                // ZPixmap
    pi.len = (uint16_t)((sizeof(pi) + bytes) / 4);
    pi.drawable = win;
    pi.gc = gc;
    pi.w = win_w;
    pi.h = rows;
    pi.x = 0;
    pi.y = y0;
    pi.left_pad = 0;
    pi.depth = root_depth;
    pi.pad = 0;
    wr(&pi, sizeof(pi));
    wr(img + (size_t)y0 * win_w, bytes);
}

// The rows of I_VideoBuffer as they were last presented, so a row that did not
// change is neither scaled nor sent. DOOM redraws its whole 320x200 buffer
// every frame — including the status bar, which changes when the player's
// health does and not otherwise — and the server's blit out of shared memory
// is about half of what a frame costs on this machine. Comparing 320 bytes a
// row is nothing next to writing 2560 and having X copy them.
static uint8_t prev[SCREENWIDTH * SCREENHEIGHT];

#include "scale3.h"


void I_FinishUpdate(void)
{
    const uint8_t *src = (const uint8_t *)I_VideoBuffer;
    int y, x, rows_per_req, y0;
    int first = -1, last = -1;
    static int last_overlay = -1, last_x = -1, last_y = -1;

    if (fbmem && org_x < 0)
        x_resolve_origin();
    int overlay = fbmem && visible && org_x >= 0 && org_y >= 0
        && org_x + win_w <= fb_w_px && org_y + win_h <= fb_h_px;
    if (overlay != last_overlay || (overlay && (org_x != last_x || org_y != last_y))) {
        // Each destination needs a complete first image, even when Doom's
        // source pixels have not changed since the previous destination.
        have_prev = 0;
        printf("overlay %s: visible=%d org=%d,%d win=%dx%d fb=%dx%d\n",
               overlay ? "ON" : "off", visible, org_x, org_y,
               win_w, win_h, fb_w_px, fb_h_px);
        fflush(stdout);
        last_overlay = overlay;
        last_x = org_x;
        last_y = org_y;
    }
    if (!overlay)
        overlay_hide();

    // Which source rows moved? (Everything, in a busy scene; the bottom 32 —
    // the status bar — almost never, and nothing at all while a menu sits
    // still.) The range is contiguous rather than per-row: one PutImage of a
    // slightly larger rectangle beats several of exactly the right ones.
    if (have_prev) {
        for (y = 0; y < SCREENHEIGHT; y++) {
            if (memcmp(src + y * SCREENWIDTH, prev + y * SCREENWIDTH, SCREENWIDTH)) {
                if (first < 0)
                    first = y;
                last = y;
            }
        }
        if (first < 0) {
            // Static menus still own their layer. A process that dies or
            // stops presenting loses the lease instead.
            overlay_publish(overlay ? 2 : 0);
            // Nothing changed at all: the frame is already on the screen.
            if (fps_report && ++frames >= 100) {
                struct timeval now;
                double dt;
                gettimeofday(&now, NULL);
                dt = (now.tv_sec - fps_t0.tv_sec) + (now.tv_usec - fps_t0.tv_usec) / 1e6;
                printf("FPS %.2f (%d frames in %.2fs)\n", frames / dt, frames, dt);
                fflush(stdout);
                frames = 0;
                fps_t0 = now;
            }
            return;
        }
    } else {
        first = 0;
        last = SCREENHEIGHT - 1;
    }
    memcpy(prev, src, SCREENWIDTH * SCREENHEIGHT);
    have_prev = 1;

    // Can this frame go straight to the screen? Only if the framebuffer is
    // mapped, the window is wholly visible, we know where it is, and it fits.
    // An 8-byte-misaligned destination (the WM's frame border makes org_x odd)
    // drops the doubled-pixel store to 32-bit halves rather than the whole
    // path to the server: two extra stores a pixel pair is nothing against
    // the copy through Xorg they replace.
    uint8_t *dst_base = overlay
        ? fbmem + (size_t)org_y * fb_stride + (size_t)org_x * 4
        : (uint8_t *)img;
    int dst_stride = overlay ? fb_stride : win_w * 4;

    // Palette lookup fused into the scale. Two horizontally-doubled pixels are
    // one 64-bit store, and the duplicated rows are the same bytes again, so a
    // scaled frame costs about one store per two output pixels and nothing
    // else per pixel.
    if (scale == 1) {
        for (y = first; y <= last; y++) {
            uint32_t *out = (uint32_t *)(dst_base + (size_t)y * dst_stride);
            const uint8_t *in = src + (size_t)y * SCREENWIDTH;
            for (x = 0; x < SCREENWIDTH; x++)
                out[x] = palette[in[x]];
        }
    } else if (scale == 2 && ((uintptr_t)dst_base % 8) == 0) {
        for (y = first; y <= last; y++) {
            uint64_t *o0 = (uint64_t *)(dst_base + (size_t)(y * 2) * dst_stride);
            uint64_t *o1 = (uint64_t *)(dst_base + (size_t)(y * 2 + 1) * dst_stride);
            const uint8_t *in = src + (size_t)y * SCREENWIDTH;
            for (x = 0; x < SCREENWIDTH; x++) {
                uint32_t c = palette[in[x]];
                uint64_t cc = ((uint64_t)c << 32) | c;
                o0[x] = cc;
                o1[x] = cc;
            }
        }
    } else if (scale == 2) {
        // the odd-org_x overlay: same fused lookup, 32-bit stores
        for (y = first; y <= last; y++) {
            uint32_t *o0 = (uint32_t *)(dst_base + (size_t)(y * 2) * dst_stride);
            uint32_t *o1 = (uint32_t *)(dst_base + (size_t)(y * 2 + 1) * dst_stride);
            const uint8_t *in = src + (size_t)y * SCREENWIDTH;
            for (x = 0; x < SCREENWIDTH; x++) {
                uint32_t c = palette[in[x]];
                o0[x * 2] = c;
                o0[x * 2 + 1] = c;
                o1[x * 2] = c;
                o1[x * 2 + 1] = c;
            }
        }
    } else if (scale == 3 && ((uintptr_t)dst_base % 8) == 0 && dst_stride % 8 == 0) {
        rbx_scale3(dst_base, (size_t)dst_stride, src, palette,
                   SCREENWIDTH, first, last);
    } else {
        int i, j;
        for (y = first; y <= last; y++) {
            uint8_t *row = dst_base + (size_t)(y * scale) * dst_stride;
            uint32_t *out = (uint32_t *)row;
            const uint8_t *in = src + (size_t)y * SCREENWIDTH;
            for (x = 0; x < SCREENWIDTH; x++) {
                uint32_t c = palette[in[x]];
                for (j = 0; j < scale; j++)
                    *out++ = c;
            }
            // the remaining scale-1 rows are byte-identical to the first
            for (i = 1; i < scale; i++)
                memcpy(row + (size_t)i * dst_stride, row, (size_t)win_w * 4);
        }
    }

    // Only the window rows the changed source rows cover — and nothing at all
    // when the pixels were written to the screen directly.
    if (overlay) {
        overlay_publish(1);
        overlay_frames++;
    } else {
        int wy0 = first * scale;
        int wh = (last - first + 1) * scale;
        x_frames++;

    if (shm_op) {
        // The frame is already where the server can see it: one small request
        // and no pixel leaves this process.
        struct {
            uint8_t op, minor;
            uint16_t len;
            uint32_t drawable, gc;
            uint16_t total_w, total_h, src_x, src_y, src_w, src_h;
            int16_t dst_x, dst_y;
            uint8_t depth, format, send_event, pad;
            uint32_t seg, offset;
        } pi;

        pi.op = shm_op;
        pi.minor = 3;            // ShmPutImage
        pi.len = 10;
        pi.drawable = win;
        pi.gc = gc;
        pi.total_w = win_w;
        pi.total_h = win_h;
        pi.src_x = 0;
        pi.src_y = wy0;
        pi.src_w = win_w;
        pi.src_h = wh;
        pi.dst_x = 0;
        pi.dst_y = wy0;
        pi.depth = root_depth;
        pi.format = 2;           // ZPixmap
        pi.send_event = 1;       // completion tells us the buffer is free again
        pi.pad = 0;
        pi.seg = shm_seg[cur_buf];
        pi.offset = 0;
        wr(&pi, sizeof(pi));
        pending++;
        // Next frame goes to the other buffer; if both are still in the
        // server's hands, wait for one back rather than painting over a frame
        // it has not finished reading.
        cur_buf = (cur_buf + 1) % NBUF;
        while (pending >= NBUF)
            I_GetEvent_blocking();
        img = shm_buf[cur_buf];
    } else {
        // Chunk by rows so no request exceeds the server's limit.
        rows_per_req = (int)(((size_t)max_request * 4 - 64) / ((size_t)win_w * 4));
        if (rows_per_req < 1)
            rows_per_req = 1;
        for (y0 = wy0; y0 < wy0 + wh; y0 += rows_per_req) {
            int rows = wy0 + wh - y0;
            if (rows > rows_per_req)
                rows = rows_per_req;
            put_rows(y0, rows);
        }
    }
    }

    if (fps_report && ++frames >= 100) {
        struct timeval now;
        double dt;
        gettimeofday(&now, NULL);
        dt = (now.tv_sec - fps_t0.tv_sec) + (now.tv_usec - fps_t0.tv_usec) / 1e6;
        printf("FPS %.2f (%d frames in %.2fs, %d direct / %d via X)\n",
               frames / dt, frames, dt, overlay_frames, x_frames);
        fflush(stdout);
        frames = 0;
        overlay_frames = x_frames = 0;
        fps_t0 = now;
    }
}

void I_ReadScreen(byte *scr)
{
    memcpy(scr, I_VideoBuffer, SCREENWIDTH * SCREENHEIGHT);
}

void I_SetPalette(byte *doompalette)
{
    int i;
    for (i = 0; i < 256; i++) {
        uint32_t r = gammatable[usegamma][*doompalette++];
        uint32_t g = gammatable[usegamma][*doompalette++];
        uint32_t b = gammatable[usegamma][*doompalette++];
        palette[i] = (r << 16) | (g << 8) | b;
    }
}

int I_GetPaletteIndex(int r, int g, int b)
{
    int best = 0, best_diff = INT_MAX, i;
    for (i = 0; i < 256; i++) {
        int dr = r - (int)((palette[i] >> 16) & 0xff);
        int dg = g - (int)((palette[i] >> 8) & 0xff);
        int db = b - (int)(palette[i] & 0xff);
        int diff = dr * dr + dg * dg + db * db;
        if (diff < best_diff) {
            best_diff = diff;
            best = i;
            if (!diff)
                break;
        }
    }
    return best;
}

// ---------------------------------------------------------------- input ---

// X keycodes on Linux are evdev codes + 8.
static int x_key_to_doom(int keycode)
{
    switch (keycode - 8) {
        case 105: return KEY_LEFTARROW;
        case 106: return KEY_RIGHTARROW;
        case 103: return KEY_UPARROW;
        case 108: return KEY_DOWNARROW;
        case 1:   return KEY_ESCAPE;
        case 28:
        case 96:  return KEY_ENTER;
        case 15:  return KEY_TAB;
        case 59:  return KEY_F1;
        case 60:  return KEY_F2;
        case 61:  return KEY_F3;
        case 62:  return KEY_F4;
        case 63:  return KEY_F5;
        case 64:  return KEY_F6;
        case 65:  return KEY_F7;
        case 66:  return KEY_F8;
        case 67:  return KEY_F9;
        case 68:  return KEY_F10;
        case 87:  return KEY_F11;
        case 88:  return KEY_F12;
        case 14:  return KEY_BACKSPACE;
        case 119: return KEY_PAUSE;
        case 13:  return KEY_EQUALS;
        case 12:  return KEY_MINUS;
        case 42:
        case 54:  return KEY_RSHIFT;
        // Ctrl is FIRE, and fbDOOM does not reach that through KEY_RCTRL:
        // m_controls.c binds key_fire to its own KEY_FIRE (0xa3), so a Ctrl
        // reported as KEY_RCTRL (0x9d) matches nothing and the gun never
        // shoots. Same story for Space below. Every other default DOES use
        // the vanilla constants (key_speed = KEY_RSHIFT, key_strafe =
        // KEY_RALT, the arrows), which is why only these two were dead.
        case 29:
        case 97:  return KEY_FIRE;
        case 56:
        case 100: return KEY_RALT;
        case 57:  return KEY_USE;   // Space opens doors and hits switches
        case 2:   return '1';
        case 3:   return '2';
        case 4:   return '3';
        case 5:   return '4';
        case 6:   return '5';
        case 7:   return '6';
        case 8:   return '7';
        case 9:   return '8';
        case 10:  return '9';
        case 11:  return '0';
        case 30:  return 'a';
        case 48:  return 'b';
        case 46:  return 'c';
        case 32:  return 'd';
        case 18:  return 'e';
        case 33:  return 'f';
        case 34:  return 'g';
        case 35:  return 'h';
        case 23:  return 'i';
        case 36:  return 'j';
        case 37:  return 'k';
        case 38:  return 'l';
        case 50:  return 'm';
        case 49:  return 'n';
        case 24:  return 'o';
        case 25:  return 'p';
        case 16:  return 'q';
        case 19:  return 'r';
        case 31:  return 's';
        case 20:  return 't';
        case 22:  return 'u';
        case 47:  return 'v';
        case 17:  return 'w';
        case 45:  return 'x';
        case 21:  return 'y';
        case 44:  return 'z';
        default:  return 0;
    }
}

// Drain what the server has sent: keys become DOOM events, ShmCompletion frees
// a shared frame, everything else is noise. Blocking mode waits for at least
// one message — the only reason to wait is that both frames are in flight.
static void x_pump(int blocking)
{
    uint8_t ev[32];
    event_t e;

    // TryRunTics polls input BEFORE I_InitGraphics has connected: xfd is
    // still 0 and every read fails. That must stay a quiet no-op — treating
    // it as "the server hung up" exited the game before its window existed.
    if (xfd <= 0)
        return;
    for (;;) {
        ssize_t n;
        int fl = fcntl(xfd, F_GETFL, 0);
        if (!blocking)
            fcntl(xfd, F_SETFL, fl | O_NONBLOCK);
        n = read(xfd, ev, sizeof(ev));
        if (!blocking)
            fcntl(xfd, F_SETFL, fl);
        if (n == 0 || (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK
                             && errno != EINTR)) {
            // The server hung up — the WM killed the window out from under
            // us (a client without WM_DELETE gets XKillClient). Nothing left
            // to draw to or hear from: leave, instead of ticking headless.
            exit(0);
        }
        if (n != (ssize_t)sizeof(ev))
            return;
        blocking = 0;   // one blocking read is enough; drain the rest cheaply
        if (shm_ev && (ev[0] & 0x7f) == shm_ev) {
            if (pending > 0)
                pending--;
            continue;
        }
        switch (ev[0] & 0x7f) {
            case 33: // ClientMessage: the WM's close button, by arrangement
                printf("x_pump: ClientMessage win=%u type=%u data0=%u (want %u/%u)\n",
                       *(uint32_t *)(ev + 4), *(uint32_t *)(ev + 8),
                       *(uint32_t *)(ev + 12), wm_protocols, wm_delete);
                if (wm_delete && *(uint32_t *)(ev + 8) == wm_protocols
                              && *(uint32_t *)(ev + 12) == wm_delete)
                    I_Quit();
                break;
            case 2:  // KeyPress
            case 3:  // KeyRelease
                e.data1 = x_key_to_doom(ev[1]);
                if (!e.data1)
                    break;
                e.type = (ev[0] & 0x7f) == 2 ? ev_keydown : ev_keyup;
                e.data2 = e.data3 = e.data4 = 0;
                D_PostEvent(&e);
                break;
            case 1:  // a reply: the rest of it follows, drain by its length
            {
                uint32_t extra = *(uint32_t *)(ev + 4);
                while (extra--) {
                    uint8_t junk[4];
                    if (rd(junk, 4) < 0)
                        return;
                }
                // The only reply this client awaits after setup is
                // TranslateCoordinates: child window at +8, then the root
                // coordinates at +12/+14 (NOT +8/+10 — that is the child id,
                // and reading it as x/y is what painted the overlay's ghost).
                if (tc_discard > 0) {
                    tc_discard--;      // answers a position we no longer hold
                    tc_pending = 0;
                } else if (tc_pending) {
                    org_x = *(int16_t *)(ev + 12);
                    org_y = *(int16_t *)(ev + 14);
                    tc_pending = 0;
                    printf("origin resolved: %d,%d\n", org_x, org_y);
                    fflush(stdout);
                }
                break;
            }
            case 15:     // VisibilityNotify
                visible = (ev[8] == 0);
                if (!visible) {
                    overlay_hide();
                    have_prev = 0;
                }
                printf("VisibilityNotify state=%d\n", ev[8]);
                fflush(stdout);
                // Obscured means the overlay is dead until the stacking
                // changes, and nothing else on this desktop will change it —
                // the appliance launches its clients once and no one raises.
                // So raise ourselves: one ConfigureWindow(stack-mode Above).
                // The WM intercepts it (SubstructureRedirect) and fluxbox
                // honors client restacks. Guarded to once per obscuring so
                // two raising clients cannot fight.
                if (ev[8] != 0) {
                    struct {
                        uint8_t op, pad;
                        uint16_t len;
                        uint32_t window;
                        uint16_t mask, pad2;
                        uint32_t value;
                    } cw = { 12, 0, 4, win, 0x40, 0, 0 }; // stack-mode Above
                    wr(&cw, sizeof(cw));
                    printf("raised window over obscurer\n");
                    fflush(stdout);
                }
                break;
            case 22:     // ConfigureNotify: the window moved or resized
                overlay_hide();
                printf("ConfigureNotify x=%d y=%d w=%u h=%u\n",
                       *(int16_t *)(ev + 16), *(int16_t *)(ev + 18),
                       *(uint16_t *)(ev + 20), *(uint16_t *)(ev + 22));
                fflush(stdout);
                org_x = org_y = -1;   // re-resolved before the next overlay frame
                if (tc_pending)
                    tc_discard++;     // any answer in flight names the old spot
                have_prev = 0;        // and repaint all of it
                break;
            case 12:     // Expose: X blanked part of us; redraw everything
                have_prev = 0;
                break;
            default:
                break;
        }
    }
}

void I_GetEvent_blocking(void)
{
    x_pump(1);
}

void I_GetEvent(void)
{
    x_pump(0);
}

void I_StartTic(void)
{
    I_GetEvent();
}

void kbd_shutdown(void) {}
void I_BeginRead(void) {}
void I_EndRead(void) {}
void I_SetWindowTitle(char *title) {}
void I_GraphicsCheckCommandLine(void) {}
void I_SetGrabMouseCallback(grabmouse_callback_t func) {}
void I_EnableLoadingDisk(void) {}
void I_BindVideoVariables(void) {}
void I_DisplayFPSDots(boolean dots_on) {}
void I_CheckIsScreensaver(void) {}
