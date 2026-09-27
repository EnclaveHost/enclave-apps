#!/usr/bin/env python3
"""Exercise the bridge's pinned route with a real TLS peer and fake credentials.

Usage: python3 tests/attested_route.py /path/to/gs-bridge
Requires Python's standard library and openssl. Uses only loopback sockets.
This checks transport enforcement, not AMD attestation (the caller verifies
attestation before supplying the expected key).
"""
import hashlib
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import tempfile
import threading


def openssl(*args, data=None):
    return subprocess.run(
        ["openssl", *args], input=data, capture_output=True, check=True
    ).stdout


with tempfile.TemporaryDirectory(prefix="risc-pinned-route-") as directory:
    root = Path(directory)
    cert, key = root / "cert.pem", root / "key.pem"
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-keyout", str(key), "-out", str(cert), "-subj", "/CN=localhost",
            "-addext", "subjectAltName=DNS:localhost")
    pub = openssl("x509", "-in", str(cert), "-pubkey", "-noout")
    der = openssl("pkey", "-pubin", "-outform", "DER", data=pub)
    pin = hashlib.sha256(der).hexdigest()

    def check(name, *, expected_pin=pin, hostname="localhost", trusted=True,
              scheme="https", succeeds=False):
        received, names, failures = [], [], []
        done = threading.Event()
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.settimeout(0.1)
        port = listener.getsockname()[1]
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, key)
        context.set_servername_callback(lambda _, server_name, __: names.append(server_name))

        def serve():
            while not done.is_set():
                try:
                    raw, _ = listener.accept()
                except socket.timeout:
                    continue
                raw.settimeout(2)
                try:
                    with context.wrap_socket(raw, server_side=True) as connection:
                        request = b""
                        while b"\r\n\r\n" not in request:
                            chunk = connection.recv(4096)
                            if not chunk:
                                break
                            request += chunk
                        received.append(request)
                        if request:
                            connection.sendall(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n"
                                b"Connection: close\r\n\r\n\x10\x20\x30"
                            )
                except (ssl.SSLError, ConnectionError, TimeoutError):
                    raw.close()  # rejected TLS peers are expected in negatives
                except Exception as error:
                    failures.append(error)
                    raw.close()

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        environment = {k: v for k, v in os.environ.items()
                       if not k.startswith(("GS_", "GSB_", "RISCBOX_", "SSL_CERT_"))}
        environment.update({
            "GS_APP_CONNECT_ADDR": f"127.0.0.1:{port}",
            "RISCBOX_API_KEY": "route-test-fake-credential",
            "SSL_CERT_DIR": str(root / "empty-trust-directory"),
            "SSL_CERT_FILE": str(cert if trusted else root / "missing-ca.pem"),
        })
        if expected_pin is not None:
            environment["GS_APP_SPKI_SHA256"] = expected_pin
        try:
            result = subprocess.run(
                [sys.argv[1], "--app", f"{scheme}://{hostname}:{port}",
                 "--frames", "raw", "--fb", "1x1", "--probe"],
                env=environment, capture_output=True, timeout=15,
            )
        finally:
            done.set()
            worker.join(3)
            listener.close()
        assert not failures, failures
        if succeeds:
            assert result.returncode == 0, result.stderr.decode()
            assert names and all(n == hostname for n in names), names
            assert any(b"route-test-fake-credential" in request for request in received)
            assert any(f"Host: {hostname}".encode() in request for request in received)
        else:
            assert result.returncode != 0, name
            assert not any(received), f"{name}: sent HTTP before accepting the peer"
        print(f"PASS {name}")

    check("attested key retains original hostname/SNI and allows authenticated request", succeeds=True)
    check("wrong key sends no HTTP or credentials", expected_pin="00" * 32)
    check("matching key does not bypass hostname validation", hostname="wrong.invalid")
    check("matching key does not bypass certificate trust", trusted=False)
    check("route without a pin fails closed", expected_pin=None)
    check("pinned plaintext route fails closed", scheme="http")
    check("malformed pin fails closed", expected_pin="invalid")
