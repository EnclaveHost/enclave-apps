"""xdoom's sound backend keeps audio in step with the picture after a stall.

A stand-in pacat stalls for a second (as a busy guest or sink does) and then
plays in real time. The mixer keeps writing on the wall clock throughout, so
without a bound the second it wrote during the stall stays queued in the pipe
for good: the 1.5 s gunshot delay measured on the KVM Box. With the bound, the
queue is back under 100 ms soon after the stall.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

APP=Path(__file__).resolve().parents[1]
XDOOM=APP/'guest/desktop/xdoom'
CFLAGS=['-std=gnu11','-O2','-Wall','-Wextra','-Werror','-Wno-unused-parameter','-DNORMALUNIX','-DLINUX','-include','music_compat.h','-I',str(XDOOM)]

HARNESS=r'''#define _GNU_SOURCE
#include <sys/ioctl.h>
#include "i_sound_rbx.c"
int W_GetNumForName(char *name) { return 0; }
int W_LumpLength(unsigned int lump) { return 0; }
void *W_CacheLumpNum(int lump, int tag) { return NULL; }
int M_snprintf(char *buf, size_t len, const char *s, ...) { return 0; }
boolean M_StringCopy(char *d, const char *s, size_t n) { return true; }
int OPL_RBX_Active(void) { return 0; }
void OPL_RBX_Render(short *s, unsigned int n) {}
int main(void) {
    struct timespec tic = { 0, 1000000000L / 35 }, start, now;
    int worst_after = 0, worst = 0;
    if (!RBX_Init(false)) return 2;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        double t;
        int queued = 0, frames;
        nanosleep(&tic, NULL);
        RBX_Update();
        clock_gettime(CLOCK_MONOTONIC, &now);
        t = (now.tv_sec - start.tv_sec) + (now.tv_nsec - start.tv_nsec) / 1e9;
        if (sink_fd < 0 || ioctl(sink_fd, FIONREAD, &queued) != 0) return 3;
        frames = queued / 4 + carry_len / 2;
        if (frames > worst) worst = frames;
        if (t > 2.0 && frames > worst_after) worst_after = frames;
        if (t > 3.0) break;
    }
    printf("%d %d\n", worst, worst_after);
    return 0;
}
'''

# Stalls for a second, then takes 11025 Hz stereo s16 in real time.
PACAT=r'''#include <time.h>
#include <unistd.h>
int main(void) {
    char buf[441];
    struct timespec stall = { 1, 0 }, step = { 0, 10000000L };
    nanosleep(&stall, NULL);
    for (;;) {
        ssize_t got = 0, r;
        while (got < (ssize_t)sizeof buf) {
            if ((r = read(0, buf + got, sizeof buf - got)) <= 0) return 0;
            got += r;
        }
        nanosleep(&step, NULL);
    }
}
'''

class SoundLag(unittest.TestCase):
    def test_backlog_drains_after_a_stall(self):
        with tempfile.TemporaryDirectory() as d:
            p=Path(d)
            (p/'harness.c').write_text(HARNESS);(p/'pacat.c').write_text(PACAT)
            subprocess.run(['cc',*CFLAGS,str(p/'harness.c'),'-o',str(p/'harness')],check=True,cwd=XDOOM)
            subprocess.run(['cc','-O2','-Wall','-Werror',str(p/'pacat.c'),'-o',str(p/'pacat')],check=True)
            r=subprocess.run([str(p/'harness')],env={'PATH':f'{d}:/usr/bin:/bin'},capture_output=True,text=True,timeout=20)
            self.assertEqual(r.returncode,0,r.stderr)
            worst,worst_after=map(int,r.stdout.split()[-2:])
            # The stall did back audio up (the check is not vacuous) ...
            self.assertGreater(worst,11025//20)
            # ... and a second later the queue is within the 100 ms bound plus
            # the one tic that may sit there between updates.
            self.assertLessEqual(worst_after,11025//10+11025//35+1,r.stdout)

if __name__=='__main__':unittest.main(verbosity=2)
