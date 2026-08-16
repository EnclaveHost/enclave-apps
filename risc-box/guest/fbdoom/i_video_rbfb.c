//
// RISC Box framebuffer video backend.
//
// The stock fbDOOM backend builds a whole-screen intermediate image and pushes
// it with a write() syscall every frame; on an emulated CPU that is most of the
// frame budget spent on pixels DOOM never drew.  This one mmaps /dev/fb0 and
// touches only the DOOM area, with the palette lookup fused into the scale, so
// a presented frame costs SCREENWIDTH*SCREENHEIGHT*scale^2 stores and nothing
// else.  Input comes straight off evdev, because the machine's keyboard is a
// virtio-input device the host feeds from the browser and there is no X here.
//
// Copyright (C) 1993-1996 Id Software, Inc.; GPLv2, like the rest of this tree.
//

#include "config.h"
#include "v_video.h"
#include "m_argv.h"
#include "d_event.h"
#include "d_main.h"
#include "i_video.h"
#include "z_zone.h"
#include "tables.h"
#include "doomkeys.h"

#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <limits.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <dirent.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <sys/types.h>
#include <linux/fb.h>
#include <linux/input.h>
#include <linux/kd.h>
#include <linux/vt.h>
#include <sys/ioctl.h>

// globals the rest of the tree expects from whichever video/input backend is
// linked in (they lived in i_video_fbdev.c and i_input_tty.c)
byte *I_VideoBuffer = NULL;
boolean screenvisible;
boolean screensaver_mode = false;
int usegamma = 0;
int usemouse = 0;
int vanilla_keyboard_mapping = 1;
float mouse_acceleration = 2.0;
int mouse_threshold = 10;
int X_width, X_height;

static struct fb_var_screeninfo vinfo;
static struct fb_fix_screeninfo finfo;
static int fd_fb = -1;
static uint8_t *fbmem;          // mmapped framebuffer
static size_t fbmem_len;
static uint8_t *fbdraw;         // first pixel of the DOOM area
static int fb_stride;           // bytes per framebuffer row
static int fb_scaling = 1;

static uint32_t palette[256];   // DOOM index -> XRGB8888

// evdev keyboards, opened once and polled per frame
#define MAX_KBD 8
static int kbd_fd[MAX_KBD];
static int kbd_n = 0;

// Frame accounting.  DOOM's own tic counter measures GUEST time, and the guest
// clock is whatever the emulator says it is, so it cannot answer "how many
// frames per real second".  This prints a wall-clock rate from gettimeofday(),
// which is the honest number when the machine's clock tracks real time.
static int frames;
static struct timeval fps_t0;
static int fps_report = 1;

// The console owns the framebuffer until told otherwise: fbcon repaints text
// (and the cursor blinks) straight over the game. KD_GRAPHICS on the active VT
// is the standard way to ask it to keep off; it is restored on the way out so a
// crash does not leave the machine with an invisible shell.
static int fd_tty = -1;

static void tty_graphics(int on)
{
    if (fd_tty < 0)
        fd_tty = open("/dev/tty0", O_RDWR);
    if (fd_tty >= 0)
        ioctl(fd_tty, KDSETMODE, on ? KD_GRAPHICS : KD_TEXT);
}

void I_ShutdownGraphics(void)
{
    tty_graphics(0);
    if (fbmem)
        munmap(fbmem, fbmem_len);
    fbmem = NULL;
    if (fd_fb >= 0)
        close(fd_fb);
    fd_fb = -1;
}

// ---------------------------------------------------------------- input ---

// Linux evdev keycode -> DOOM key.  Only what DOOM binds; anything else is
// dropped rather than guessed at.
static int evdev_to_doom(int code)
{
    switch (code) {
        case KEY_LEFT:      return KEY_LEFTARROW;
        case KEY_RIGHT:     return KEY_RIGHTARROW;
        case KEY_UP:        return KEY_UPARROW;
        case KEY_DOWN:      return KEY_DOWNARROW;
        case KEY_ESC:       return KEY_ESCAPE;
        case KEY_ENTER:
        case KEY_KPENTER:   return KEY_ENTER;
        case KEY_TAB:       return KEY_TAB;
        case KEY_F1:        return KEY_F1;
        case KEY_F2:        return KEY_F2;
        case KEY_F3:        return KEY_F3;
        case KEY_F4:        return KEY_F4;
        case KEY_F5:        return KEY_F5;
        case KEY_F6:        return KEY_F6;
        case KEY_F7:        return KEY_F7;
        case KEY_F8:        return KEY_F8;
        case KEY_F9:        return KEY_F9;
        case KEY_F10:       return KEY_F10;
        case KEY_F11:       return KEY_F11;
        case KEY_F12:       return KEY_F12;
        case KEY_BACKSPACE: return KEY_BACKSPACE;
        case KEY_PAUSE:     return KEY_PAUSE;
        case KEY_EQUAL:     return KEY_EQUALS;
        case KEY_MINUS:     return KEY_MINUS;
        case KEY_LEFTSHIFT:
        case KEY_RIGHTSHIFT: return KEY_RSHIFT;
        case KEY_LEFTCTRL:
        case KEY_RIGHTCTRL: return KEY_RCTRL;     // ctrl = fire
        case KEY_LEFTALT:
        case KEY_RIGHTALT:  return KEY_RALT;
        case KEY_SPACE:     return ' ';           // space = use
        case KEY_1:         return '1';
        case KEY_2:         return '2';
        case KEY_3:         return '3';
        case KEY_4:         return '4';
        case KEY_5:         return '5';
        case KEY_6:         return '6';
        case KEY_7:         return '7';
        case KEY_8:         return '8';
        case KEY_9:         return '9';
        case KEY_0:         return '0';
        case KEY_A:         return 'a';
        case KEY_B:         return 'b';
        case KEY_C:         return 'c';
        case KEY_D:         return 'd';
        case KEY_E:         return 'e';
        case KEY_F:         return 'f';
        case KEY_G:         return 'g';
        case KEY_H:         return 'h';
        case KEY_I:         return 'i';
        case KEY_J:         return 'j';
        case KEY_K:         return 'k';
        case KEY_L:         return 'l';
        case KEY_M:         return 'm';
        case KEY_N:         return 'n';
        case KEY_O:         return 'o';
        case KEY_P:         return 'p';
        case KEY_Q:         return 'q';
        case KEY_R:         return 'r';
        case KEY_S:         return 's';
        case KEY_T:         return 't';
        case KEY_U:         return 'u';
        case KEY_V:         return 'v';
        case KEY_W:         return 'w';
        case KEY_X:         return 'x';
        case KEY_Y:         return 'y';
        case KEY_Z:         return 'z';
        default:            return 0;
    }
}

int I_InitInput(void)
{
    DIR *d = opendir("/dev/input");
    struct dirent *e;

    if (!d) {
        printf("I_InitInput: no /dev/input\n");
        return 0;
    }
    while ((e = readdir(d)) && kbd_n < MAX_KBD) {
        char path[64];
        unsigned long bits = 0;
        int fd;

        if (strncmp(e->d_name, "event", 5) != 0)
            continue;
        snprintf(path, sizeof(path), "/dev/input/%s", e->d_name);
        fd = open(path, O_RDONLY | O_NONBLOCK);
        if (fd < 0)
            continue;
        // keep only devices that report keys; the pointer's stream would
        // otherwise be read and discarded every frame for nothing
        if (ioctl(fd, EVIOCGBIT(0, sizeof(bits)), &bits) < 0 ||
            !(bits & (1UL << EV_KEY))) {
            close(fd);
            continue;
        }
        kbd_fd[kbd_n++] = fd;
        printf("I_InitInput: keyboard on %s\n", path);
    }
    closedir(d);
    return kbd_n;
}

void I_GetEvent(void)
{
    struct input_event ie[32];
    int i, k;

    for (k = 0; k < kbd_n; k++) {
        for (;;) {
            ssize_t n = read(kbd_fd[k], ie, sizeof(ie));
            if (n <= 0)
                break;
            for (i = 0; i < (int)(n / sizeof(ie[0])); i++) {
                event_t ev;
                if (ie[i].type != EV_KEY)
                    continue;
                ev.data1 = evdev_to_doom(ie[i].code);
                if (!ev.data1)
                    continue;
                // value 2 is autorepeat: DOOM wants the key held, not retyped
                if (ie[i].value == 2)
                    continue;
                ev.type = ie[i].value ? ev_keydown : ev_keyup;
                ev.data2 = ev.data3 = ev.data4 = 0;
                D_PostEvent(&ev);
            }
            if (n < (ssize_t)sizeof(ie))
                break;
        }
    }
}

void I_StartTic(void)
{
    I_GetEvent();
}

// --------------------------------------------------------------- output ---

void I_InitGraphics(void)
{
    int i, xoff, yoff;

    fd_fb = open("/dev/fb0", O_RDWR);
    if (fd_fb < 0) {
        printf("I_InitGraphics: cannot open /dev/fb0: %s\n", strerror(errno));
        exit(1);
    }
    if (ioctl(fd_fb, FBIOGET_VSCREENINFO, &vinfo) < 0 ||
        ioctl(fd_fb, FBIOGET_FSCREENINFO, &finfo) < 0) {
        printf("I_InitGraphics: fb ioctl failed: %s\n", strerror(errno));
        exit(1);
    }
    if (vinfo.bits_per_pixel != 32) {
        printf("I_InitGraphics: need a 32bpp framebuffer, got %d\n",
               vinfo.bits_per_pixel);
        exit(1);
    }
    fb_stride = finfo.line_length;
    fbmem_len = (size_t)fb_stride * vinfo.yres;
    fbmem = mmap(NULL, fbmem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd_fb, 0);
    if (fbmem == MAP_FAILED) {
        printf("I_InitGraphics: mmap failed: %s\n", strerror(errno));
        exit(1);
    }

    i = M_CheckParmWithArgs("-scaling", 1);
    if (i > 0) {
        fb_scaling = atoi(myargv[i + 1]);
    } else {
        fb_scaling = vinfo.xres / SCREENWIDTH;
        if ((int)vinfo.yres / SCREENHEIGHT < fb_scaling)
            fb_scaling = vinfo.yres / SCREENHEIGHT;
    }
    if (fb_scaling < 1)
        fb_scaling = 1;

    // centre the image; every frame then writes the same rectangle
    xoff = ((int)vinfo.xres - SCREENWIDTH * fb_scaling) / 2;
    yoff = ((int)vinfo.yres - SCREENHEIGHT * fb_scaling) / 2;
    fbdraw = fbmem + (size_t)yoff * fb_stride + (size_t)xoff * 4;

    printf("I_InitGraphics: fb %dx%d %dbpp stride %d, scale %d, offset %d,%d\n",
           vinfo.xres, vinfo.yres, vinfo.bits_per_pixel, fb_stride,
           fb_scaling, xoff, yoff);

    tty_graphics(1);
    memset(fbmem, 0, fbmem_len);   // black out whatever the console left

    I_VideoBuffer = (byte *)Z_Malloc(SCREENWIDTH * SCREENHEIGHT, PU_STATIC, NULL);
    screenvisible = true;

    if (M_CheckParm("-nofps"))
        fps_report = 0;
    gettimeofday(&fps_t0, NULL);

    I_InitInput();
}

void I_StartFrame(void) {}
void I_UpdateNoBlit(void) {}

//
// One presented frame: palette-map I_VideoBuffer into the framebuffer.
//
// The scale-1 and scale-2 paths are written out because they are the two the
// machine actually runs, and the generic loop's inner branch costs more than
// the pixel work at these sizes.
//
void I_FinishUpdate(void)
{
    const uint8_t *src = (const uint8_t *)I_VideoBuffer;
    uint8_t *dst = fbdraw;
    int y, x;

    if (fb_scaling == 1) {
        for (y = 0; y < SCREENHEIGHT; y++) {
            uint32_t *out = (uint32_t *)dst;
            for (x = 0; x < SCREENWIDTH; x++)
                out[x] = palette[src[x]];
            src += SCREENWIDTH;
            dst += fb_stride;
        }
    } else if (fb_scaling == 2) {
        for (y = 0; y < SCREENHEIGHT; y++) {
            uint64_t *o0 = (uint64_t *)dst;
            uint64_t *o1 = (uint64_t *)(dst + fb_stride);
            // two horizontally-doubled pixels are one aligned 64-bit store,
            // and the row below is the same bytes again
            for (x = 0; x < SCREENWIDTH; x++) {
                uint32_t c = palette[src[x]];
                uint64_t cc = ((uint64_t)c << 32) | c;
                o0[x] = cc;
                o1[x] = cc;
            }
            src += SCREENWIDTH;
            dst += fb_stride * 2;
        }
    } else {
        int i, j;
        for (y = 0; y < SCREENHEIGHT; y++) {
            for (i = 0; i < fb_scaling; i++) {
                uint32_t *out = (uint32_t *)(dst + (size_t)i * fb_stride);
                for (x = 0; x < SCREENWIDTH; x++) {
                    uint32_t c = palette[src[x]];
                    for (j = 0; j < fb_scaling; j++)
                        *out++ = c;
                }
            }
            src += SCREENWIDTH;
            dst += (size_t)fb_stride * fb_scaling;
        }
    }

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
        palette[i] = (r << 16) | (g << 8) | b;   // x8r8g8b8
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
            if (diff == 0)
                break;
        }
    }
    return best;
}

void kbd_shutdown(void)
{
    int i;
    for (i = 0; i < kbd_n; i++)
        close(kbd_fd[i]);
    kbd_n = 0;
}

void I_BeginRead(void) {}
void I_EndRead(void) {}
void I_SetWindowTitle(char *title) {}
void I_GraphicsCheckCommandLine(void) {}
void I_SetGrabMouseCallback(grabmouse_callback_t func) {}
void I_EnableLoadingDisk(void) {}
void I_BindVideoVariables(void) {}
void I_DisplayFPSDots(boolean dots_on) {}
void I_CheckIsScreensaver(void) {}
