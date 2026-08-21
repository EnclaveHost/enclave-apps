// risc-box: a SYNCHRONOUS OPL driver for the RISC Box sound path.
//
// Chocolate Doom's OPL backends are callback-driven: an audio thread asks for
// samples and the driver advances the chip inside that callback. This machine
// has no audio thread — i_sound_rbx.c owns the one PCM stream, meters it off
// the wall clock, and writes it to aplay. So the chip is advanced from THERE,
// pulled rather than pushed, through OPL_RBX_Render().
//
// The timing model is the part worth stating. i_oplmusic schedules MIDI events
// by asking for a callback `us` microseconds from now (OPL_SetCallback), so
// the renderer cannot simply generate N samples and then fire whatever is due:
// a note that should land a third of the way through the buffer would arrive
// late by the rest of it, and at the 100 ms buffers this stream uses that is
// audible as smeared timing. Instead each Render call walks the buffer in
// chunks bounded by the NEXT due callback — generate up to it, fire it, carry
// on — so events land on the sample they were scheduled for.
//
// THE SYNTHESIS IS NOT HERE. An earlier cut ran Nuked OPL3 in the guest and
// it cost 21% of DOOM's frame rate (median 69.2 -> 54.7 fps), because a
// cycle-accurate 49716 Hz chip on an emulated RV64 core with no vector unit is
// the worst case for that machine. So register writes go to the emulator's
// mailbox at 0x10006000 and the host generates the samples as native code,
// mixing them into the card's PCM. What stays in the guest is the cheap half:
// MUS-to-MIDI, GENMIDI instruments, and the event clock below.

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include "opl.h"
#include "opl_queue.h"

// The emulator's OPL register mailbox (emu/src/device/opl.rs). The host owns
// the chip; this side only posts register writes and keeps MIDI time.
#define OPL_MMIO_BASE 0x10006000UL
#define OPL_REG_INDEX_LO 0x00
#define OPL_REG_INDEX_HI 0x01
#define OPL_REG_DATA     0x04

static volatile uint8_t *mmio = NULL;
static int mmio_tried = 0;

// The mixer's rate. i_sound_rbx.c plays 11025 Hz stereo, and the chip is
// resampled to whatever this is set to.
static unsigned int mix_rate = 11025;

static int chip_ready = 0;
static int paused = 0;

static opl_callback_queue_t *queue = NULL;

/// Microseconds of chip time consumed so far. Callback deadlines are absolute
/// against this, so it must advance by exactly what is rendered — drift here
/// is drift in the music's tempo.
static uint64_t current_time = 0;
/// Sample fraction carried between calls. A render of N samples is
/// N*1e6/rate microseconds, which is not an integer at 11025 Hz; dropping the
/// remainder every call would run the music progressively sharp. (The sound
/// card learned this same lesson the hard way — see virtio_snd::accrue.)
static uint64_t time_frac = 0;

/// Map the host's register mailbox. Userspace can do this directly: the guest
/// kernel is built CONFIG_DEVMEM=y with STRICT_DEVMEM off, and this is device
/// space rather than RAM — the same access a DOS program had to 0x388.
static int MapMmio(void)
{
    int fd;
    long page;
    void *p;

    if (mmio != NULL) { return 1; }
    if (mmio_tried) { return 0; }
    mmio_tried = 1;

    fd = open("/dev/mem", O_RDWR | O_SYNC);
    if (fd < 0)
    {
        printf("I_OPL: /dev/mem unavailable (%s); music disabled\n", strerror(errno));
        return 0;
    }
    page = sysconf(_SC_PAGESIZE);
    p = mmap(NULL, (size_t) page, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
             (off_t) OPL_MMIO_BASE);
    close(fd);
    if (p == MAP_FAILED)
    {
        printf("I_OPL: cannot map the OPL port (%s); music disabled\n", strerror(errno));
        return 0;
    }
    mmio = (volatile uint8_t *) p;
    printf("I_OPL: host OPL3 at 0x%lx\n", OPL_MMIO_BASE);
    return 1;
}

opl_init_result_t OPL_Init(unsigned int port_base)
{
    (void) port_base;
    if (queue == NULL)
    {
        queue = OPL_Queue_Create();
    }
    if (!MapMmio())
    {
        return OPL_INIT_NONE;
    }
    chip_ready = 1;
    current_time = 0;
    time_frac = 0;
    paused = 0;
    // OPL3: i_oplmusic uses the extra voices when the chip reports one.
    return OPL_INIT_OPL3;
}

void OPL_Shutdown(void)
{
    if (queue != NULL)
    {
        OPL_Queue_Destroy(queue);
        queue = NULL;
    }
    chip_ready = 0;
}

void OPL_SetSampleRate(unsigned int rate)
{
    // The host generates at the sound card's rate, which it already knows;
    // nothing to do but remember it for the MIDI clock below.
    mix_rate = rate;
}

void OPL_WriteRegister(int reg, int value)
{
    if (!chip_ready || mmio == NULL)
    {
        return;
    }
    // Index low, index high (the high byte selects OPL3's upper bank), then
    // the value — writing the value is what commits the pair on the host.
    mmio[OPL_REG_INDEX_LO] = (uint8_t) (reg & 0xff);
    mmio[OPL_REG_INDEX_HI] = (uint8_t) ((reg >> 8) & 0xff);
    mmio[OPL_REG_DATA] = (uint8_t) value;
}

void OPL_WritePort(opl_port_t port, unsigned int value)
{
    // Register/data port pairs are what a DOS driver would use; i_oplmusic
    // goes through OPL_WriteRegister, so this only has to exist.
    static int selected = 0;
    if (port == OPL_REGISTER_PORT || port == OPL_REGISTER_PORT_OPL3)
    {
        selected = (int) value;
    }
    else
    {
        OPL_WriteRegister(selected, (int) value);
    }
}

opl_init_result_t OPL_Detect(void)
{
    return OPL_INIT_OPL3;
}

void OPL_InitRegisters(int opl3)
{
    int r;
    // Silence every operator, then enable OPL3 mode if asked. Chocolate Doom
    // does this through the port interface; the effect is what matters.
    for (r = 0x40; r <= 0x55; ++r)  { OPL_WriteRegister(r, 0x3f); }
    for (r = 0x60; r <= 0xf5; ++r)  { OPL_WriteRegister(r, 0x00); }
    for (r = 0x01; r <= 0x0f; ++r)  { OPL_WriteRegister(r, 0x00); }
    if (opl3)
    {
        OPL_WriteRegister(0x105, 0x01);   // OPL3 enable
        for (r = 0x140; r <= 0x155; ++r) { OPL_WriteRegister(r, 0x3f); }
        for (r = 0x160; r <= 0x1f5; ++r) { OPL_WriteRegister(r, 0x00); }
    }
}

void OPL_SetCallback(uint64_t us, opl_callback_t callback, void *data)
{
    if (queue != NULL)
    {
        OPL_Queue_Push(queue, callback, data, current_time + us);
    }
}

void OPL_ClearCallbacks(void)
{
    if (queue != NULL)
    {
        OPL_Queue_Clear(queue);
    }
}

void OPL_AdjustCallbacks(float factor)
{
    if (queue != NULL)
    {
        OPL_Queue_AdjustCallbacks(queue, current_time, factor);
    }
}

// Single-threaded: the mixer and the music run on DOOM's own thread, so
// there is nothing to lock against.
void OPL_Lock(void) {}
void OPL_Unlock(void) {}

void OPL_SetPaused(int pause_music)
{
    paused = pause_music;
}

void OPL_Delay(uint64_t us)
{
    // Only used by hardware detection paths, which this driver does not take.
    (void) us;
}

/// Advance MIDI time by `frames` samples' worth and fire every event that
/// falls due, posting its register writes to the host.
///
/// This does NOT generate audio: the host owns the chip and sums the music
/// into the card's PCM (src/opl.rs). What stays here is the part that must
/// keep DOOM's own timing — i_oplmusic schedules events in microseconds from
/// now, and those deadlines are only meaningful against the same clock the
/// sound stream runs on.
///
/// Time still advances in the chunks the events fall into rather than one
/// jump per call, because a note scheduled a third of the way through a
/// buffer must have its registers written before the ones after it: the host
/// applies writes in the order it receives them.
void OPL_RBX_Tick(unsigned int frames)
{
    unsigned int done = 0;

    if (!chip_ready || paused)
    {
        // A paused song holds its place: time must not advance, or every
        // pending event fires at once on resume.
        return;
    }

    while (done < frames)
    {
        unsigned int chunk = frames - done;

        if (queue != NULL && !OPL_Queue_IsEmpty(queue))
        {
            uint64_t next = OPL_Queue_Peek(queue);
            if (next <= current_time)
            {
                opl_callback_t cb;
                void *data;
                if (OPL_Queue_Pop(queue, &cb, &data))
                {
                    cb(data);
                }
                continue;
            }
            {
                uint64_t until = ((next - current_time) * mix_rate) / 1000000ULL;
                if (until < chunk)
                {
                    chunk = (unsigned int) until;
                }
                if (chunk == 0)
                {
                    chunk = 1; // always make progress
                }
            }
        }

        done += chunk;

        // Advance by exactly what the card played, carrying the remainder so
        // the tempo does not drift. (The sound card learned this the hard
        // way — see virtio_snd::accrue.)
        {
            uint64_t num = (uint64_t) chunk * 1000000ULL + time_frac;
            current_time += num / mix_rate;
            time_frac = num % mix_rate;
        }
    }
}

/// Whether a song is actually playing, so the mixer can skip the MIDI work
/// when there is nothing to schedule.
int OPL_RBX_Active(void)
{
    return chip_ready && !paused && queue != NULL && !OPL_Queue_IsEmpty(queue);
}
