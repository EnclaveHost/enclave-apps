/* Fixed X11 screenshot helper, executed only as desktop UID1000. No paths,
 * output formats, displays or commands are accepted as arguments. The parent
 * supplies a fixed DISPLAY/XAUTHORITY and bounds runtime/output. */
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static int u32(uint32_t n) {
    unsigned char b[4] = { n >> 24, n >> 16, n >> 8, n };
    return fwrite(b, 1, 4, stdout) == 4 ? 0 : -1;
}
static unsigned char channel(unsigned long pixel, unsigned long mask) {
    if (!mask) return 0;
    while (!(mask & 1)) { mask >>= 1; pixel >>= 1; }
    return (unsigned char)(((uint64_t)(pixel & mask) * 255) / mask);
}
int main(int argc, char **argv) {
    (void)argv;
    if (argc != 1 || getuid() != 1000 || geteuid() != 1000) return 1;
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    Window root = DefaultRootWindow(d), child, queried;
    XWindowAttributes a;
    int x = -1, y = -1, wx, wy;
    unsigned int buttons;
    if (!XGetWindowAttributes(d, root, &a) || a.width < 1 || a.height < 1 ||
        a.width > 3840 || a.height > 2160 || a.visual->class != TrueColor) { XCloseDisplay(d); return 1; }
    XImage *im = XGetImage(d, root, 0, 0, (unsigned)a.width, (unsigned)a.height, AllPlanes, ZPixmap);
    if (!im) { XCloseDisplay(d); return 1; }
    if (!XQueryPointer(d, root, &queried, &child, &x, &y, &wx, &wy, &buttons)) x = y = -1;
    int bad = fwrite("RGB1",1,4,stdout)!=4 || u32(a.width) || u32(a.height) || u32(x) || u32(y);
    unsigned char *row = malloc((size_t)a.width * 3);
    if (!row) bad = 1;
    for (int py = 0; py < a.height && !bad; py++) {
        for (int px = 0; px < a.width; px++) {
            unsigned long p = XGetPixel(im,px,py);
            row[px*3] = channel(p,im->red_mask);
            row[px*3+1] = channel(p,im->green_mask);
            row[px*3+2] = channel(p,im->blue_mask);
        }
        if (fwrite(row,1,(size_t)a.width*3,stdout)!=(size_t)a.width*3) bad = 1;
    }
    free(row); XDestroyImage(im); XCloseDisplay(d);
    return bad || fflush(stdout) ? 1 : 0;
}
