#!/usr/bin/env python3
"""Pair the local Moonlight client with the owner-authenticated development VM.

Moonlight's `pair` command leaves a success dialog open. Process exit is not
proof of success; an authenticated application listing is. Private logs stay
in the supplied work directory and PINs never appear in errors from this tool.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import time
import urllib.error
import urllib.request

ADDRESS = '127.0.0.2'
APP = 'KVM Box Desktop'


def without_host(original, address):
    lines = original.splitlines(keepends=True)
    try:
        start = lines.index('[hosts]\n') + 1
    except ValueError:
        return original
    end = next((i for i in range(start, len(lines)) if lines[i].startswith('[')), len(lines))
    groups, other = {}, []
    for line in lines[start:end]:
        match = re.match(r'(\d+)\\([\s\S]*)', line)
        if match:
            groups.setdefault(match[1], []).append(match[2])
        elif not line.startswith('size='):
            other.append(line)
    kept = [group for group in groups.values()
            if not any(line.strip() == 'manualaddress=' + address for line in group)]
    replacement = [str(i) + '\\' + line for i, group in enumerate(kept, 1) for line in group]
    replacement += ['size=' + str(len(kept)) + '\n'] + other
    return ''.join(lines[:start] + replacement + lines[end:])


def private_file(path):
    return os.fdopen(os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), 'w')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work', required=True, type=Path)
    parser.add_argument('--port', type=int, default=34719)
    parser.add_argument('--replace-host-record', action='store_true',
                        help='Use only after booting a new measured image with a new Sunshine identity.')
    args = parser.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    lock = (args.work / 'pair-local.lock').open('w')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    for p in Path('/proc').iterdir():
        if not p.name.isdigit():
            continue
        try:
            argv = (p / 'cmdline').read_bytes().split(b'\0')
            if len(argv) > 2 and Path(os.fsdecode(argv[0])).name == 'moonlight' and argv[1:3] == [b'pair', ADDRESS.encode()]:
                raise SystemExit('Another Moonlight pairing client is still running for this VM; close it first.')
        except OSError:
            pass
    stamp = str(time.time_ns())
    list_log = args.work / ('moonlight-list-' + stamp + '.private.log')
    with private_file(list_log) as log:
        def authenticated_list():
            log.flush()
            try:
                result = subprocess.run(['moonlight', 'list', ADDRESS], stdout=subprocess.PIPE,
                                        stderr=subprocess.STDOUT, timeout=8)
            except subprocess.TimeoutExpired:
                return False
            text = result.stdout.decode(errors='replace')
            log.write(text); log.flush()
            return result.returncode == 0 and APP in text
        if authenticated_list():
            print('Moonlight pairing verified by authenticated application listing.')
            return
        if args.replace_host_record:
            config = Path.home() / '.config/Moonlight Game Streaming Project/Moonlight.conf'
            original = config.read_text()
            with private_file(args.work / ('Moonlight-' + stamp + '.private.conf')) as backup:
                backup.write(original)
            config.write_text(without_host(original, ADDRESS))
        pin = str(secrets.randbelow(9000) + 1000)
        with private_file(args.work / ('moonlight-pair-' + stamp + '.private.log')) as pair_log:
            process = subprocess.Popen(['moonlight', 'pair', ADDRESS, '--pin', pin],
                                       stdout=pair_log, stderr=pair_log)
            try:
                deadline = time.monotonic() + 60
                admitted = False
                while time.monotonic() < deadline:
                    if not admitted:
                        request = urllib.request.Request(
                            f'http://127.0.0.1:{args.port}/desktop/api/moonlight/pair',
                            data=json.dumps({'pin': pin, 'name': 'Local Moonlight'}).encode(),
                            headers={'Content-Type': 'application/json', 'X-KVM-Box-Request': '1'})
                        try:
                            with urllib.request.urlopen(request, timeout=5) as response:
                                answer = json.load(response)
                            admitted = answer.get('status') is True or answer.get('ok') is True
                        except (OSError, ValueError):
                            pass
                    elif authenticated_list():
                        print('Moonlight paired and verified by authenticated application listing.')
                        return
                    if process.poll() is not None:
                        break
                    time.sleep(1)
                raise SystemExit('Pairing was not verified. Private diagnostics retained; no second pairing was started.')
            finally:
                # This also closes the CLI's success dialog after verification.
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill(); process.wait()


if __name__ == '__main__':
    main()
