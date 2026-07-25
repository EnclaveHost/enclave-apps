/* spincube — the RISC Box graphics demo: a spinning cube with a procedural
 * texture and diffuse lighting, rendered in software into an Xlib window.
 *
 * Deliberately plain: no GL, no extensions beyond core X11 (XPutImage, no
 * MIT-SHM — one less thing to go wrong on a fresh server), float math (the
 * emulator's RV64D got fixed for exactly this kind of guest), affine texture
 * mapping and Lambert lighting done per pixel. On the ~29 MIPS emulated CPU
 * a 320x320 window turns a frame every second or two — a time-lapse spin,
 * which is the point: two /fb.png snapshots a few seconds apart differ, and
 * that difference IS the end-to-end proof the desktop renders.
 *
 * Build: riscv64-linux-gnu-gcc -O2 -o spincube cube.c -lX11 -lm
 */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define W 320
#define H 320
#define TEX 64

static unsigned int tex[TEX * TEX];

static void make_texture(void) {
    /* warm checkerboard with a thin grid line — obviously a texture, cheap */
    for (int y = 0; y < TEX; y++)
        for (int x = 0; x < TEX; x++) {
            int c = ((x >> 3) ^ (y >> 3)) & 1;
            unsigned int base = c ? 0xd8a24a : 0x3d67a8; /* amber / steel */
            if ((x & 7) == 0 || (y & 7) == 0) base = 0x22314a;
            tex[y * TEX + x] = base;
        }
}

typedef struct { double x, y, z; } V3;

static V3 rot(V3 p, double ax, double ay) {
    double ca = cos(ax), sa = sin(ax), cb = cos(ay), sb = sin(ay);
    V3 q, r;
    q.x = p.x; q.y = ca * p.y - sa * p.z; q.z = sa * p.y + ca * p.z; /* X axis */
    r.x = cb * q.x + sb * q.z; r.y = q.y; r.z = -sb * q.x + cb * q.z; /* Y axis */
    return r;
}

/* one textured, lit triangle with affine mapping; flat z for painter sort */
static void tri(unsigned int *fb, double zbuf_unused,
                double x0, double y0, double u0, double v0,
                double x1, double y1, double u1, double v1,
                double x2, double y2, double u2, double v2, double light) {
    (void)zbuf_unused;
    int minx = (int)fmax(0, floor(fmin(x0, fmin(x1, x2))));
    int maxx = (int)fmin(W - 1, ceil(fmax(x0, fmax(x1, x2))));
    int miny = (int)fmax(0, floor(fmin(y0, fmin(y1, y2))));
    int maxy = (int)fmin(H - 1, ceil(fmax(y0, fmax(y1, y2))));
    double d = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
    if (fabs(d) < 1e-9) return;
    for (int y = miny; y <= maxy; y++)
        for (int x = minx; x <= maxx; x++) {
            double w1 = ((x - x0) * (y2 - y0) - (x2 - x0) * (y - y0)) / d;
            double w2 = ((x1 - x0) * (y - y0) - (x - x0) * (y1 - y0)) / d;
            double w0 = 1.0 - w1 - w2;
            if (w0 < 0 || w1 < 0 || w2 < 0) continue;
            double u = w0 * u0 + w1 * u1 + w2 * u2;
            double v = w0 * v0 + w1 * v1 + w2 * v2;
            int tx = (int)(u * (TEX - 1)) & (TEX - 1);
            int ty = (int)(v * (TEX - 1)) & (TEX - 1);
            unsigned int t = tex[ty * TEX + tx];
            int r = (int)(((t >> 16) & 0xff) * light);
            int g = (int)(((t >> 8) & 0xff) * light);
            int b = (int)((t & 0xff) * light);
            fb[y * W + x] = (unsigned)(r << 16 | g << 8 | b);
        }
}

int main(void) {
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "spincube: no display\n"); return 1; }
    int scr = DefaultScreen(dpy);
    Window win = XCreateSimpleWindow(dpy, RootWindow(dpy, scr), 40, 40, W, H, 1,
                                     BlackPixel(dpy, scr), 0x101820);
    XStoreName(dpy, win, "spincube — textured, lit, emulated");
    /* claim a user-specified position: the X server has no input devices, so
     * a window manager must never ask the (nonexistent) pointer to place this
     * window — USPosition makes twm map it where we said, no interaction */
    XSizeHints hints;
    memset(&hints, 0, sizeof(hints));
    hints.flags = USPosition | USSize;
    hints.x = 40; hints.y = 40;
    hints.width = W; hints.height = H;
    XSetWMNormalHints(dpy, win, &hints);
    XSelectInput(dpy, win, ExposureMask);
    XMapWindow(dpy, win);
    GC gc = DefaultGC(dpy, scr);
    unsigned int *fb = malloc((size_t)W * H * 4);
    XImage *img = XCreateImage(dpy, DefaultVisual(dpy, scr), 24, ZPixmap, 0,
                               (char *)fb, W, H, 32, 0);
    make_texture();

    static const V3 verts[8] = {
        {-1, -1, -1}, {1, -1, -1}, {1, 1, -1}, {-1, 1, -1},
        {-1, -1, 1},  {1, -1, 1},  {1, 1, 1},  {-1, 1, 1},
    };
    /* faces as vertex indices, counter-clockwise seen from outside */
    static const int faces[6][4] = {
        {0, 1, 2, 3}, {5, 4, 7, 6}, {4, 0, 3, 7},
        {1, 5, 6, 2}, {4, 5, 1, 0}, {3, 2, 6, 7},
    };
    const V3 lightdir = {0.37, -0.56, -0.74}; /* normalized-ish, from over the shoulder */

    double ax = 0.5, ay = 0.3;
    for (unsigned long frame = 0;; frame++) {
        for (int i = 0; i < W * H; i++) fb[i] = 0x101820;
        V3 r[8];
        for (int i = 0; i < 8; i++) r[i] = rot(verts[i], ax, ay);
        /* painter sort: draw faces back to front by mean z */
        int order[6] = {0, 1, 2, 3, 4, 5};
        double depth[6];
        for (int f = 0; f < 6; f++) {
            depth[f] = 0;
            for (int k = 0; k < 4; k++) depth[f] += r[faces[f][k]].z;
        }
        for (int a = 0; a < 5; a++)
            for (int b = a + 1; b < 6; b++)
                if (depth[order[a]] > depth[order[b]]) {
                    int t = order[a]; order[a] = order[b]; order[b] = t;
                }
        for (int oi = 0; oi < 6; oi++) {
            int f = order[oi];
            const int *ix = faces[f];
            /* face normal from two edges (after rotation) */
            V3 e1 = {r[ix[1]].x - r[ix[0]].x, r[ix[1]].y - r[ix[0]].y, r[ix[1]].z - r[ix[0]].z};
            V3 e2 = {r[ix[3]].x - r[ix[0]].x, r[ix[3]].y - r[ix[0]].y, r[ix[3]].z - r[ix[0]].z};
            V3 n = {e1.y * e2.z - e1.z * e2.y, e1.z * e2.x - e1.x * e2.z, e1.x * e2.y - e1.y * e2.x};
            double nl = sqrt(n.x * n.x + n.y * n.y + n.z * n.z);
            if (nl < 1e-9) continue;
            if (n.z >= 0) continue; /* backface (camera looks down -z at +z) */
            double lambert = -(n.x * lightdir.x + n.y * lightdir.y + n.z * lightdir.z) / nl;
            if (lambert < 0) lambert = 0;
            double light = 0.25 + 0.75 * lambert; /* ambient + diffuse */
            /* perspective project */
            double px[4], py[4];
            for (int k = 0; k < 4; k++) {
                double z = r[ix[k]].z + 4.2;
                px[k] = W / 2.0 + (r[ix[k]].x / z) * W * 1.1;
                py[k] = H / 2.0 + (r[ix[k]].y / z) * H * 1.1;
            }
            tri(fb, 0, px[0], py[0], 0, 0, px[1], py[1], 1, 0, px[2], py[2], 1, 1, light);
            tri(fb, 0, px[0], py[0], 0, 0, px[2], py[2], 1, 1, px[3], py[3], 0, 1, light);
        }
        XPutImage(dpy, win, gc, img, 0, 0, 0, 0, W, H);
        XFlush(dpy);
        ax += 0.13;
        ay += 0.09;
        /* drain events so the server never blocks on us */
        while (XPending(dpy)) { XEvent ev; XNextEvent(dpy, &ev); }
        usleep(50000); /* pacing is moot at emulated speed; be a good citizen */
    }
}
