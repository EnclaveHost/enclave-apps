#!/usr/bin/env python3
"""Bounded readiness protocol checks with a real Unix socket and synthetic replies."""
import base64
import importlib.util
import json
import os
from pathlib import Path
import socket
import struct
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

NATIVE = Path(__file__).resolve().parents[1]/'guest/native'
spec = importlib.util.spec_from_file_location('readiness', NATIVE/'readiness.py')
r = importlib.util.module_from_spec(spec); spec.loader.exec_module(r)
build_spec = importlib.util.spec_from_file_location('root_build', NATIVE/'build_root_image.py')
build = importlib.util.module_from_spec(build_spec); build_spec.loader.exec_module(build)


def jpeg_header(w=960, h=600):
    # A structural fixture, deliberately not asserted to be a decodable image.
    sof = bytes([8])+struct.pack('>HH', h, w)+bytes([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1])
    sos = bytes([3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0])
    return b'\xff\xd8\xff\xc0'+struct.pack('>H',len(sof)+2)+sof+b'\xff\xda'+struct.pack('>H',len(sos)+2)+sos+b'\x01\xff\xd9'


class Readiness(unittest.TestCase):
    def test_jpeg_dimensions_and_malformed_headers(self):
        r.jpeg_dimensions(jpeg_header(), 960, 600)
        for data,w,h in [(b'\xff\xd8\xff\xd9',960,600),(jpeg_header(),100,600),
                         (jpeg_header()[:-3],960,600),(b'not JPEG',960,600),
                         (b'\xff\xd8\xff\xc0\xff\xff\xff\xd9',960,600)]:
            with self.assertRaises(r.NotReady): r.jpeg_dimensions(data,w,h)

    def test_http_bounds_and_ambiguity(self):
        good=b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}'
        self.assertEqual(r.parse_http(good,2),(b'application/json',b'{}'))
        for bad in [good.replace(b'200',b'503'),good.replace(b'Length: 2',b'Length: 3'),
                    good.replace(b'Content-Length:',b'Transfer-Encoding: chunked\r\nContent-Length:'),
                    good.replace(b'Content-Length:',b'Content-Type: text/html\r\nContent-Length:')]:
            with self.assertRaises(r.NotReady):r.parse_http(bad,128)
        with self.assertRaises(r.NotReady):r.parse_http(good,1)
        with self.assertRaises(r.NotReady):r.strict_json(b'{"ok":true,"ok":false}')

    @unittest.skipUnless(os.getuid()==1000 and os.getgid()==1000,'production socket metadata requires uid/gid 1000')
    def test_real_socket_fixed_command_then_screenshot(self):
        with tempfile.TemporaryDirectory() as directory:
            os.chmod(directory,0o750)
            path=directory+'/desktop.sock'
            listener=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
            listener.bind(path);os.chmod(path,0o660);listener.listen(2);listener.settimeout(2)
            seen=[];errors=[]
            responses=[{'ok':True,'exitCode':0,'output':'native-automation-ready','truncated':False},
                       {'ok':True,'mime':'image/jpeg','screen':{'width':960,'height':600},'image':base64.b64encode(jpeg_header()).decode()}]
            def serve():
                try:
                    for value in responses:
                        connection,_=listener.accept()
                        with connection:
                            connection.settimeout(2);data=b''
                            while b'\r\n\r\n' not in data:data+=connection.recv(4096)
                            header,body=data.split(b'\r\n\r\n',1)
                            length=int(next(x.split(b':',1)[1] for x in header.split(b'\r\n') if x.lower().startswith(b'content-length:')))
                            while len(body)<length:body+=connection.recv(length-len(body))
                            seen.append((header.split(b'\r\n')[0],json.loads(body)))
                            response=json.dumps(value).encode()
                            connection.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: '+str(len(response)).encode()+b'\r\nConnection: close\r\n\r\n'+response)
                except Exception as e:errors.append(e)
            thread=threading.Thread(target=serve);thread.start()
            original=r.pinned_directory
            def pinned(path,uid,gid,mode):
                self.assertEqual(path,'/run/native-automation')
                return original(directory,uid,gid,mode)
            try:
                with patch.object(r,'pinned_directory',side_effect=pinned):r.automation()
            finally:listener.close();thread.join(3)
            self.assertFalse(thread.is_alive());self.assertEqual(errors,[])
            self.assertEqual(seen[0][0],b'POST /exec HTTP/1.1')
            self.assertEqual(seen[0][1]['cmd'],'printf native-automation-ready')
            self.assertEqual(seen[1][0],b'POST /computer HTTP/1.1')
            self.assertEqual(seen[1][1],{'action':'screenshot','quality':30})

    def test_image_schema_requires_pinned_automation_pair(self):
        old=build.files_for_schema('kvm-box-native-root-build/1')
        new=build.files_for_schema('kvm-box-native-root-build/2')
        self.assertEqual(set(new)-set(old),{'automation','capture'})
        self.assertEqual(new['automation'],('usr/local/libexec/kvm-box/automation',0o555))
        self.assertEqual(new['capture'],('usr/local/libexec/kvm-box/capture',0o555))
        with self.assertRaises(ValueError):build.files_for_schema('kvm-box-native-root-build/3')

    def test_mode_requires_isolated_interpreter_and_ui_role(self):
        self.assertEqual(r.PROBES['automation'][0],1003)
        with patch.object(r.os,'getuid',return_value=1000),patch.object(r.os,'geteuid',return_value=1000):
            self.assertNotEqual(r.main(['readiness.py','automation']),0)


if __name__=='__main__':unittest.main()
