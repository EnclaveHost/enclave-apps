ARG DESKTOP_IMAGE=enclave-kvm-box-desktop:0.3.0
FROM debian@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS sandbox-build
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev libseccomp-dev
COPY shield/guest-sandbox.c /build/guest-sandbox.c
RUN cc -O2 -Wall -Wextra -Werror /build/guest-sandbox.c -lseccomp -Wl,-z,relro,-z,now -o /build/shield-sandbox

FROM ${DESKTOP_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends python3-numpy nftables libseccomp2 libgomp1 libssl3t64 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -M -u 1001 -g 1000 -s /usr/sbin/nologin shield \
    && dpkg-query -W > /package-versions.txt
COPY --from=sandbox-build /build/shield-sandbox /usr/local/bin/shield-sandbox
COPY sdk /opt/shield
COPY shield /shield
COPY policy /shield/policy
RUN chmod 755 /shield/guest-start.sh /shield/mount-models.sh \
    && printf '#!/bin/sh\nexec /usr/bin/python3 -I /shield/client.py "$@"\n' > /usr/local/bin/shield-compute \
    && chmod 755 /usr/local/bin/shield-compute
# Standard CLI access replaces the two diagnostic desktop menu entries.
RUN sed -i '/\[exec\] (Shield Compute)/d; /\[exec\] (Native Shield Compute)/d' /home/desktop/.fluxbox/menu
