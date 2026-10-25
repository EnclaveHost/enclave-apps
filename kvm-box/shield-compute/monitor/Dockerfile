FROM enclave-kvm-shield-native-builder:1
RUN apt-get update && apt-get install -y --no-install-recommends libncurses-dev libjson-c-dev pkg-config \
    && dpkg-query -W > /monitor-package-versions.txt && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["/bin/sh"]
