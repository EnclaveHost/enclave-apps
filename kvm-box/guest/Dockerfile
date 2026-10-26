FROM debian@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS desktop-build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential libx11-dev
COPY desktop/xdoom /build/xdoom
RUN make -C /build/xdoom -j4 CROSS_COMPILE= NOSDL=1 CFLAGS="-O2 -DNORMALUNIX -DLINUX -include music_compat.h" && strip /build/xdoom/fbdoom
COPY desktop/risc-xwin.c /build/risc-xwin.c
RUN cc -Os -Wall -Wextra /build/risc-xwin.c -lX11 -o /build/risc-xwin-native

FROM debian@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    busybox-static firefox-esr fluxbox xterm xfonts-base x11-xserver-utils x11vnc xdotool x11-utils \
    xserver-xorg-core xserver-xorg-video-dummy xserver-xorg-input-libinput udev \
    novnc websockify dbus-x11 fonts-dejavu-core ca-certificates python3 nodejs curl pulseaudio pulseaudio-utils \
    && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/* \
    && useradd -m -u 1000 -s /bin/sh desktop \
    && dpkg-query -W > /package-versions.txt

# Official Sunshine release, pinned independently of the package repository.
RUN curl -fL 'https://github.com/LizardByte/Sunshine/releases/download/v2026.914.233613/sunshine_2026.914.233613-1%2Bdebiantrixie_amd64.deb' -o /tmp/sunshine.deb \
    && echo 'a86f858cbae57f60ecebd445402ee0295c1c057b857bd45d44c2ea2d255cbbfb  /tmp/sunshine.deb' | sha256sum -c - \
    && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends /tmp/sunshine.deb \
    && rm -rf /var/lib/apt/lists/* /tmp/sunshine.deb \
    && dpkg-query -W > /package-versions.txt

RUN usermod -aG input desktop

COPY --from=desktop-build /build/xdoom/fbdoom /usr/local/bin/xdoom
COPY --from=desktop-build /build/risc-xwin-native /usr/local/bin/risc-xwin-native
COPY desktop/home /home/desktop/
COPY desktop/bin /usr/local/bin/
COPY desktop/games /usr/share/games/doom/
COPY desktop/styles/bloe /usr/share/fluxbox/styles/bloe/
COPY desktop/PORTING.txt /desktop-porting.txt
RUN ln -s /usr/local/bin/doom-fs /usr/bin/doom-fs && chmod 755 /usr/local/bin/risc-browser /usr/local/bin/doom-fs && chown -R 1000:1000 /home/desktop && fc-cache -f
COPY desktop/xdoom/COPYING desktop/xdoom/COPYING.opl3 /usr/share/doc/kvm-box/
COPY desktop/firefox/ublock-origin.xpi /usr/share/kvm-box/ublock-origin.xpi
COPY desktop/firefox/policies.json /usr/lib/firefox-esr/distribution/policies.json
