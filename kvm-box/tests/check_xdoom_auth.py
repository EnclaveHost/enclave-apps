"""Exercise the raw X11 client's cookie parser using synthetic public data."""
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import unittest

APP=Path(__file__).resolve().parents[1]
COOKIE=bytes(range(16))
def record(family=256,address=None,display=b'0',protocol=b'MIT-MAGIC-COOKIE-1',data=COOKIE):
    if address is None: address=socket.gethostname().encode()
    return struct.pack('>H',family)+b''.join(struct.pack('>H',len(x))+x for x in [address,display,protocol,data])

class Auth(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build=tempfile.TemporaryDirectory()
        p=Path(cls.build.name)
        (p/'probe.c').write_text('''#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include "x11-auth.h"
int main(int argc,char **argv) {
    if(argc!=2)return 64;
    unsigned char cookie[16];for(int i=0;i<16;i++)cookie[i]=0xa5;
    int r=x_auth_load(atoi(argv[1]),cookie);
    if(r==1)for(int i=0;i<16;i++)if(cookie[i]!=(unsigned char)i)return 65;
    printf("%d\\n",r);return 0;
}
''')
        cls.exe=p/'probe'
        subprocess.run(['cc','-O2','-Wall','-Wextra','-Werror','-I',str(APP/'guest/desktop/xdoom'),str(p/'probe.c'),'-o',str(cls.exe)],check=True)
    @classmethod
    def tearDownClass(cls):cls.build.cleanup()
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.path=Path(self.tmp.name)/'Xauthority'
        self.write(record())
    def write(self,data):
        self.path.write_bytes(data);self.path.chmod(0o600)
    def load(self,expected,path=None,display=0,absent=False):
        env={'PATH':'/usr/bin:/bin'}
        if not absent:env['XAUTHORITY']=str(self.path) if path is None else path
        r=subprocess.run([str(self.exe),str(display)],env=env,capture_output=True,text=True,timeout=2)
        self.assertEqual(r.returncode,0,r.stderr)
        self.assertEqual(r.stdout,str(expected)+'\n')
    def test_local_and_wild(self):
        self.load(1);self.write(record(family=65535,address=b''));self.load(1)
    def test_absent_only_legacy_fallback(self):
        self.load(0,absent=True);self.load(-1,path='');self.load(-1,path='relative');self.load(-1,path=str(self.path)+'-missing')
    def test_modes_links_and_fifo(self):
        self.path.chmod(0o644);self.load(-1);self.path.chmod(0o600)
        link=self.path.with_name('link');link.symlink_to(self.path);self.load(-1,path=str(link));link.unlink()
        os.link(self.path,link);self.load(-1);link.unlink()
        self.path.unlink();os.mkfifo(self.path,0o600);self.load(-1)
    def test_truncations(self):
        good=record()
        for n in range(len(good)):
            self.write(good[:n]);self.load(-1)
        self.write(good+b'\x00');self.load(-1)
    def test_no_matching_identity(self):
        for data in [record(address=b'wrong-host'),record(display=b'1'),record(display=b'00'),record(protocol=b'unknown'),record(data=b'bad'),record(family=0)]:
            self.write(data);self.load(-1)
    def test_conflicts_and_unrelated_records(self):
        self.write(record()+record(data=b'x'*16));self.load(-1)
        self.write(record(display=b'1')+record());self.load(1)
        self.write(record()+record());self.load(1)
    def test_bounds(self):
        for data in [b'x'*65537,record(address=b'x'*4097),record(display=b'1')*65]:
            self.write(data);self.load(-1)
        self.write(record());self.load(-1,display=-1);self.load(-1,display=65536)

if __name__=='__main__':unittest.main(verbosity=2)
