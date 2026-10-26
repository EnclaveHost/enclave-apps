#!/usr/bin/env python3
"""Download the CTS and dependencies pinned by cts-lock.json into a new directory."""
import argparse
import hashlib
import json
from pathlib import Path
import tarfile
import urllib.request


def unpack(record, archive, staging, destination):
    digest = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(record['url'], timeout=120) as response, archive.open('xb') as out:
        while data := response.read(1024*1024):
            size += len(data)
            if size > record['bytes']:
                raise RuntimeError('Archive exceeds pinned size')
            digest.update(data)
            out.write(data)
    if size != record['bytes'] or digest.hexdigest() != record['sha256']:
        raise RuntimeError('Archive does not match cts-lock.json: '+record['url'])
    staging.mkdir()
    with tarfile.open(archive) as tf:
        tf.extractall(staging, filter='data')
    entries = list(staging.iterdir())
    if len(entries) != 1 or not entries[0].is_dir():
        raise RuntimeError('Expected one archive root')
    destination.parent.mkdir(parents=True, exist_ok=True)
    entries[0].rename(destination)
    staging.rmdir()


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--output', type=Path, required=True)
    a = p.parse_args()
    lock = json.loads(Path(__file__).with_name('cts-lock.json').read_text())
    root = a.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    archives = root/'archives'
    archives.mkdir()
    unpack(lock['source'], archives/'source.tar.gz', root/'extract-source', root/'source')
    for record in lock['dependencies']:
        name = record['name']
        unpack(record, archives/(name+'.tar.gz'), root/('extract-'+name), root/'source/external'/name/'src')
        print(name, record['revision'], flush=True)
    (root/'source.json').write_text(json.dumps(lock['source'], indent=2)+'\n')
    (root/'dependencies.json').write_text(json.dumps(lock['dependencies'], indent=2)+'\n')


if __name__ == '__main__':
    main()
