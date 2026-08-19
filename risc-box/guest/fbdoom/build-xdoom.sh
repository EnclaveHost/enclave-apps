set -e
apt-get update -qq && apt-get install -y -qq gcc-riscv64-linux-gnu make git ca-certificates python3 > /dev/null
git clone -q --depth 1 https://github.com/maximevince/fbDOOM /fb
cd /fb/fbdoom
cp /src/i_video_x11raw.c .
# x11raw does video AND input: replace both fbdev video and tty input objects
sed -i 's/i_video_fbdev.o/i_video_x11raw.o/' Makefile
sed -i '/i_input_tty.o/d' Makefile
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
