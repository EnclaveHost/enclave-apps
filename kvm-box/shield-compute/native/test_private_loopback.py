#!/usr/bin/env python3
"""Exercise real nftables in an isolated container network namespace.

Requires root/CAP_NET_ADMIN and --network none. Never run with host networking.
Only synthetic loopback data is exchanged; no GPU/model/service access.
"""
import json
import multiprocessing as mp
import os
from pathlib import Path
import socket
import subprocess
import sys


def server(pipe, uid, address):
    os.setgroups([]);os.setgid(1000);os.setuid(uid)
    try:
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
            listener.bind((address,0));listener.listen();listener.settimeout(3)
            pipe.send(listener.getsockname()[1])
            with listener.accept()[0] as peer:
                peer.settimeout(2)
                if peer.recv(16)==b'public-fixture':peer.sendall(b'public-reply')
    except OSError:pass
    finally:pipe.close()


def client(pipe,uid,address,port):
    os.setgroups([]);os.setgid(1000);os.setuid(uid)
    try:
        with socket.socket() as sock:
            sock.settimeout(1);sock.bind(('127.0.0.1',0));sock.connect((address,port))
            sock.sendall(b'public-fixture');result=sock.recv(16)==b'public-reply'
    except OSError:result=False
    pipe.send(result);pipe.close()


def exchange(server_uid,client_uid,address):
    parent,child=mp.Pipe(duplex=False);s=mp.Process(target=server,args=(child,server_uid,address));s.start();child.close()
    assert parent.poll(3),'listener failed';port=parent.recv();parent.close()
    parent,child=mp.Pipe(duplex=False);c=mp.Process(target=client,args=(child,client_uid,address,port));c.start();child.close()
    assert parent.poll(3),'client timed out';result=parent.recv();parent.close();c.join(3);s.join(4)
    for process in (s,c):
        if process.is_alive():process.terminate();process.join();raise AssertionError('probe did not finish')
        assert process.exitcode==0
    return result


assert os.getuid()==0
# A host network namespace has non-loopback interfaces. Refuse to alter it.
assert set(os.listdir('/sys/class/net'))=={'lo'},'requires a separate network-none namespace'
text=Path('/tests/guest-firewall.nft').read_text().replace('__ATTESTED_SERVICE_PORTS__','9742')
results={}
try:
    for mode in ('baseline','fixed'):
        rules=text if mode=='fixed' else text.replace('        ip saddr 127.0.0.3 reject\n','')
        subprocess.run(['nft','flush','ruleset'],check=True)
        subprocess.run(['nft','-f','-'],input=rules,text=True,check=True)
        checks={'servicePrivate':exchange(1001,1001,'127.0.0.3'),
                'desktopOwn':exchange(1000,1000,'127.0.0.1'),
                'desktopToPrivate':exchange(1001,1000,'127.0.0.3'),
                'spoofedPrivateServer':exchange(1000,1001,'127.0.0.3')}
        assert checks=={'servicePrivate':True,'desktopOwn':True,'desktopToPrivate':False,'spoofedPrivateServer':mode=='baseline'},checks
        results[mode]=checks
    results['passed']=True
finally:
    subprocess.run(['nft','flush','ruleset'],check=True)
    Path('/evidence/firewall.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results))
