//
// RISC Box sound backend for fbDOOM's NOSDL build.
//
// fbDOOM compiled without SDL has an EMPTY sound_modules[] table, so
// sound_module stays NULL and every I_* sound call is a no-op — the reason the
// game was silent even after the machine grew a sound card. This is a real
// sound_module_t: it mixes DOOM's sfx itself and writes the result to the card.
//
// It reaches the card through `aplay` rather than libasound. xdoom is linked
// STATIC against glibc (the image is musl, so static is what makes it run at
// all), and a static glibc build cannot pick up the musl libasound.so.2 that
// ships in the image. aplay is already there, it is the one process that
// speaks ALSA, and a pipe costs nothing next to what the mixer does.
//
// The output rate is 11025 Hz, which is DOOM's own sfx rate: the common case
// then needs no resampling at all, only the handful of 22 kHz lumps do. Stereo,
// because DOOM's positional audio is a left/right pan and mono would throw it
// away.
//
// TIMING: writes are metered off the wall clock, not off "one tic's worth per
// call". S_UpdateSounds calls Update() once per tic, but a tic on this machine
// is not reliably 1/35 s, and feeding a fixed 315 frames per call makes the
// audio drift against the game exactly as much as the tic rate does. Asking
// the clock how much time has passed keeps sound and picture together, and the
// pipe is non-blocking so a slow moment costs a glitch rather than stalling the
// game loop waiting on the card.

#include <errno.h>
#include <signal.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "doomtype.h"
#include "i_sound.h"
#include "m_misc.h"
#include "w_wad.h"
#include "z_zone.h"

#define OUT_RATE      11025
#define OUT_CHANNELS  2
#define NUM_CHANNELS  8
/// Never mix more than this in one go: after a long stall the clock reports a
/// huge debt, and paying all of it would be a burst of stale audio.
#define MAX_FRAMES    2048

typedef struct
{
    boolean active;
    const unsigned char *data; // 8-bit unsigned samples
    unsigned int length;       // samples
    unsigned int pos;          // 16.16 fixed point into `data`
    unsigned int step;         // 16.16 increment per output frame
    int leftvol;               // 0..127
    int rightvol;
} rbx_channel_t;

static rbx_channel_t channels[NUM_CHANNELS];
static FILE *sink;
static int sink_fd = -1;
static boolean use_sfx_prefix_local;
static struct timespec last_write;
static int reopen_countdown;
static short mixbuf[MAX_FRAMES * OUT_CHANNELS];

static void SplitVolume(int vol, int sep, int *left, int *right)
{
    // sep is 0 (hard left) .. 255 (hard right), 128 centre.
    if (sep < 0) sep = 0;
    if (sep > 255) sep = 255;
    *left = (vol * (255 - sep)) / 255;
    *right = (vol * sep) / 255;
}

/// (Re)start the player. Split out because it is also the recovery path: on a
/// machine this slow aplay can lose its stream to an underrun and exit, and a
/// game that then goes permanently silent is worse than one that skips a
/// sound. Failing here is not fatal — the next Update tries again.
static boolean OpenSink(void)
{
    // -q so aplay's chatter stays out of the game's console. Reading raw from
    // stdin means no header and no seeking, which is all a mixer can offer.
    sink = popen("aplay -q -f S16_LE -r 11025 -c 2 -t raw - 2>/dev/null", "w");
    if (sink == NULL)
    {
        return false;
    }
    sink_fd = fileno(sink);
    // Non-blocking: the game loop must never wait on the card.
    fcntl(sink_fd, F_SETFL, fcntl(sink_fd, F_GETFL, 0) | O_NONBLOCK);
    clock_gettime(CLOCK_MONOTONIC, &last_write);
    return true;
}

static boolean RBX_Init(boolean use_sfx_prefix)
{
    use_sfx_prefix_local = use_sfx_prefix;
    memset(channels, 0, sizeof(channels));

    // Without this a write to a dead aplay kills DOOM instead of returning
    // EPIPE, turning "sound stopped" into "the game exited".
    signal(SIGPIPE, SIG_IGN);

    if (!OpenSink())
    {
        printf("I_Sound: could not start aplay; sound disabled\n");
        return false;
    }
    printf("I_Sound: RISC Box sound, %d Hz stereo via aplay\n", OUT_RATE);
    return true;
}

static void RBX_Shutdown(void)
{
    if (sink != NULL)
    {
        pclose(sink);
        sink = NULL;
        sink_fd = -1;
    }
}

static int RBX_GetSfxLumpNum(sfxinfo_t *sfx)
{
    char namebuf[9];

    if (use_sfx_prefix_local)
    {
        M_snprintf(namebuf, sizeof(namebuf), "ds%s", sfx->name);
    }
    else
    {
        M_StringCopy(namebuf, sfx->name, sizeof(namebuf));
    }

    return W_GetNumForName(namebuf);
}

/// Point `chan` at a DMX sound lump. The format is an 8-byte header (format,
/// sample rate, sample count) followed by 8-bit unsigned samples, with 16 pad
/// samples at each end that must not be played.
static boolean LoadLump(rbx_channel_t *chan, int lumpnum)
{
    const unsigned char *data;
    unsigned int lumplen, length, rate;

    lumplen = W_LumpLength(lumpnum);
    if (lumplen < 8)
    {
        return false;
    }
    data = W_CacheLumpNum(lumpnum, PU_STATIC);
    if (data == NULL || (data[0] | (data[1] << 8)) != 3)
    {
        return false;
    }
    rate = data[2] | (data[3] << 8);
    length = data[4] | (data[5] << 8) | (data[6] << 16) | ((unsigned)data[7] << 24);
    // A header claiming more than the lump holds is a broken lump, not a long
    // sound.
    if (length > lumplen - 8 || length <= 48)
    {
        return false;
    }
    if (rate == 0)
    {
        rate = OUT_RATE;
    }

    chan->data = data + 8 + 16;
    chan->length = length - 32;
    chan->pos = 0;
    // 16.16 step: 1.0 when the lump is already at the output rate.
    chan->step = (unsigned int)(((unsigned long long)rate << 16) / OUT_RATE);
    return true;
}

static int RBX_StartSound(sfxinfo_t *sfxinfo, int channel, int vol, int sep)
{
    rbx_channel_t *chan;

    if (sink == NULL || channel < 0 || channel >= NUM_CHANNELS)
    {
        return -1;
    }
    chan = &channels[channel];
    chan->active = false;
    if (!LoadLump(chan, sfxinfo->lumpnum))
    {
        return -1;
    }
    SplitVolume(vol, sep, &chan->leftvol, &chan->rightvol);
    chan->active = true;
    return channel;
}

static void RBX_StopSound(int channel)
{
    if (channel >= 0 && channel < NUM_CHANNELS)
    {
        channels[channel].active = false;
    }
}

static boolean RBX_SoundIsPlaying(int channel)
{
    if (channel < 0 || channel >= NUM_CHANNELS)
    {
        return false;
    }
    return channels[channel].active;
}

static void RBX_UpdateSoundParams(int channel, int vol, int sep)
{
    if (channel >= 0 && channel < NUM_CHANNELS)
    {
        SplitVolume(vol, sep, &channels[channel].leftvol,
                    &channels[channel].rightvol);
    }
}

/// How many output frames the passage of real time has earned us.
static int FramesDue(void)
{
    struct timespec now;
    long long ns;
    int frames;

    clock_gettime(CLOCK_MONOTONIC, &now);
    ns = (long long)(now.tv_sec - last_write.tv_sec) * 1000000000LL
         + (now.tv_nsec - last_write.tv_nsec);
    if (ns <= 0)
    {
        return 0;
    }
    frames = (int)((ns * OUT_RATE) / 1000000000LL);
    if (frames > MAX_FRAMES)
    {
        // Debt past this point is stale audio nobody wants to hear late.
        frames = MAX_FRAMES;
        clock_gettime(CLOCK_MONOTONIC, &last_write);
        return frames;
    }
    // Only bank the time actually consumed, so the remainder is not lost and
    // the rate stays exact over many calls.
    ns = (long long)frames * 1000000000LL / OUT_RATE;
    last_write.tv_nsec += ns % 1000000000LL;
    last_write.tv_sec += ns / 1000000000LL;
    if (last_write.tv_nsec >= 1000000000LL)
    {
        last_write.tv_nsec -= 1000000000LL;
        last_write.tv_sec += 1;
    }
    return frames;
}

static void RBX_Update(void)
{
    int frames, i, c;
    ssize_t written;

    if (sink == NULL)
    {
        // Retry the player now and then rather than never. Cheap: one popen a
        // second at worst, and only while sound is meant to be running.
        if (++reopen_countdown < 64)
        {
            return;
        }
        reopen_countdown = 0;
        if (!OpenSink())
        {
            return;
        }
    }
    frames = FramesDue();
    if (frames <= 0)
    {
        return;
    }

    memset(mixbuf, 0, sizeof(short) * frames * OUT_CHANNELS);

    for (c = 0; c < NUM_CHANNELS; ++c)
    {
        rbx_channel_t *chan = &channels[c];

        if (!chan->active)
        {
            continue;
        }
        for (i = 0; i < frames; ++i)
        {
            unsigned int index = chan->pos >> 16;
            int sample;

            if (index >= chan->length)
            {
                chan->active = false;
                break;
            }
            // 8-bit unsigned centred on 128 -> signed 16-bit.
            sample = ((int)chan->data[index] - 128) * 256;
            // >>7 rather than /127: a division per sample per channel is real
            // money on a ~130 MIPS emulated core, and 128 vs 127 is under a
            // percent of volume — inaudible, and not worth an emulated divide.
            mixbuf[i * 2] += (short)((sample * chan->leftvol) >> 7);
            mixbuf[i * 2 + 1] += (short)((sample * chan->rightvol) >> 7);
            chan->pos += chan->step;
        }
    }

    // A short write is fine and a full pipe is fine: aplay drains at the card's
    // real rate, so anything we cannot hand over now is audio we would have
    // been late with anyway.
    written = write(sink_fd, mixbuf, (size_t)frames * OUT_CHANNELS * sizeof(short));
    if (written < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR)
    {
        // aplay exited (an underrun it could not recover, or the card went
        // away). Drop the pipe and let the retry above bring sound back.
        pclose(sink);
        sink = NULL;
        sink_fd = -1;
        reopen_countdown = 0;
    }
}

static void RBX_CacheSounds(sfxinfo_t *sounds, int num_sounds)
{
    // Lumps are cached on first use in LoadLump; nothing to do up front.
    (void)sounds;
    (void)num_sounds;
}

static snddevice_t sound_rbx_devices[] =
{
    SNDDEVICE_SB,
    SNDDEVICE_PAS,
    SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER,
    SNDDEVICE_SOUNDCANVAS,
    SNDDEVICE_AWE32,
};

sound_module_t sound_rbx_module =
{
    sound_rbx_devices,
    arrlen(sound_rbx_devices),
    RBX_Init,
    RBX_Shutdown,
    RBX_GetSfxLumpNum,
    RBX_Update,
    RBX_UpdateSoundParams,
    RBX_StartSound,
    RBX_StopSound,
    RBX_SoundIsPlaying,
    RBX_CacheSounds,
};
