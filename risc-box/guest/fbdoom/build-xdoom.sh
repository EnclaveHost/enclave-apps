set -e
apt-get update -qq && apt-get install -y -qq gcc-riscv64-linux-gnu make git ca-certificates python3 patch > /dev/null
git clone -q --depth 1 https://github.com/maximevince/fbDOOM /fb
cd /fb/fbdoom
cp /src/i_video_x11raw.c .
cp /src/i_sound_rbx.c .
# uncapped rendering with world-state interpolation (-uncapped, -fpsmax N):
# render between the 35 Hz tics, blending mobj/view/psprite state by the
# wall-clock sub-tic phase, so the presented rate is bounded by the machine.
patch -p2 < /src/uncapped.patch
# MUSIC. fbDOOM keeps i_oplmusic.c but strips everything under it: no mus2mid,
# no midifile, no memio, no opl/ at all. Those come from chocolate-doom
# (same GPL2 lineage) in /src/music, together with opl_rbx.c — a synchronous
# OPL driver, because this machine has no audio thread to drive a callback
# backend. See music/opl_rbx.c for the timing model.
cp /src/music/*.c /src/music/*.h .
# The vendored sources come from a much newer chocolate-doom than fbDOOM
# forked from, and use PACKED_STRUCT(...) which fbDOOM's doomtype.h predates
# (it only ever had PACKEDATTR). Add the macro rather than rewriting every
# struct, so the vendored files stay byte-identical to upstream and are
# trivial to re-sync.
python3 - <<'PACKEOF'
s = open("doomtype.h").read()
if "PACKED_STRUCT" not in s:
    s = s.replace("#endif", '''
#ifndef PACKED_STRUCT
#ifdef __GNUC__
#define PACKED_STRUCT(...) struct __attribute__((packed)) __VA_ARGS__
#else
#define PACKED_STRUCT(...) __pragma(pack(push,1)) struct __VA_ARGS__ __pragma(pack(pop))
#endif
#endif

#endif''', 1) if s.count("#endif") else s
    open("doomtype.h", "w").write(s)
PACKEOF
grep -c PACKED_STRUCT doomtype.h
# The vendored i_oplmusic.c is modern chocolate-doom and expects a few
# declarations fbDOOM's i_sound.h predates. Add them rather than editing the
# vendored file, for the same re-sync reason as PACKED_STRUCT above.
python3 - <<'SNDHEOF'
s = open("i_sound.h").read()
if "opl_driver_ver_t" not in s:
    s = s.replace("#endif", '''
typedef enum {
    opl_doom1_1_666,    // Doom 1 v1.666
    opl_doom2_1_666,    // Doom 2 v1.666, Hexen, Heretic
    opl_doom_1_9        // Doom v1.9, Strife
} opl_driver_ver_t;

void I_SetOPLDriverVer(opl_driver_ver_t ver);
void I_OPL_DevMessages(char *, size_t);

#endif''', 1)
    open("i_sound.h", "w").write(s)
SNDHEOF
grep -c opl_driver_ver_t i_sound.h
# x11raw does video AND input: replace both fbdev video and tty input objects
sed -i 's/i_video_fbdev.o/i_video_x11raw.o/' Makefile
sed -i '/i_input_tty.o/d' Makefile
# the sound backend is ours too: NOSDL leaves sound_modules[] empty, so the
# engine has nowhere to send audio until this is both built AND registered
sed -i 's/i_sound.o/i_sound.o i_sound_rbx.o mus2mid.o midifile.o opl_queue.o opl_rbx.o i_oplmusic.o music_compat.o/' Makefile
python3 - <<'SNDEOF'
s = open("i_sound.c").read()
s = s.replace("extern sound_module_t sound_sdl_module;",
              "extern sound_module_t sound_sdl_module;\nextern sound_module_t sound_rbx_module;")
old = """static sound_module_t *sound_modules[] = 
{
#ifdef FEATURE_SOUND
    &sound_sdl_module,
    &sound_pcsound_module,
#endif
    NULL,
};"""
new = """static sound_module_t *sound_modules[] = 
{
#ifdef FEATURE_SOUND
    &sound_sdl_module,
    &sound_pcsound_module,
#endif
    &sound_rbx_module,
    NULL,
};"""
assert old in s, "sound_modules[] table not found -- upstream changed"
s = s.replace(old, new)
open("i_sound.c", "w").write(s)
SNDEOF
# register the OPL music module: like sound_modules[], NOSDL leaves
# music_modules[] empty, so the engine has nowhere to send music until this
# entry exists no matter what is linked in.
python3 - <<'MUSEOF'
s = open("i_sound.c").read()
s = s.replace("extern music_module_t music_opl_module;", "")
s = s.replace("extern sound_module_t sound_rbx_module;",
              "extern sound_module_t sound_rbx_module;\nextern music_module_t music_opl_module;")
old = """static music_module_t *music_modules[] =
{
#ifdef FEATURE_SOUND
    &music_sdl_module,
    &music_opl_module,
#endif
    NULL,
};"""
new = """static music_module_t *music_modules[] =
{
#ifdef FEATURE_SOUND
    &music_sdl_module,
#endif
    &music_opl_module,
    NULL,
};"""
assert old in s, "music_modules[] table not found -- upstream changed"
s = s.replace(old, new)
open("i_sound.c", "w").write(s)
MUSEOF
grep -n 'OBJS' Makefile | head -3
# upstream hardcodes homedir=/mnt for its appliance; the desktop image wants $HOME
sed -i 's|homedir = "/mnt"|homedir = getenv("HOME")|' m_config.c
# upstream's I_Quit only exits under ORIGCODE (the SDL build): in the NOSDL
# build quit ran the exit funcs and RETURNED, so QUIT GAME played on. Move
# the exit outside the guard.
python3 - <<'PYEOF'
s = open("i_system.c").read()
s = s.replace("#if ORIGCODE\n    SDL_Quit();\n\n    exit(0);\n#endif\n}",
              "#if ORIGCODE\n    SDL_Quit();\n#endif\n\n    exit(0);\n}")
open("i_system.c", "w").write(s)
assert s.count("    exit(0);\n}") >= 1
PYEOF
make -s CROSS_COMPILE=riscv64-linux-gnu- NOSDL=1 CFLAGS="-O2 -static -DNORMALUNIX -DLINUX" LDFLAGS="-static" 2>&1 | tail -5
cp fbdoom /src/xdoom && echo BUILD-OK
