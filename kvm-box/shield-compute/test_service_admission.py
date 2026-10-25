"""Real Unix-stream pressure tests for the service's bounded handler pool."""
import concurrent.futures
import os
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from service import Handler, Server, receive, send


class LimitedServer(Server):
    max_clients = 2
    def __init__(self,*args,**kwargs):
        self.entered=threading.Event()
        super().__init__(*args,**kwargs)
    def process_request(self,*args):
        self.entered.set()
        super().process_request(*args)


class PublicFixtureCompute:
    def __init__(self,gate=None):
        self.config={'clientUids':[os.getuid()]}
        self.lock=threading.Lock()
        self.current=self.peak=self.calls=0
        self.gate=gate
        self.two_active=threading.Event()
        self.retired=False
    def run(self,request):
        with self.lock:
            self.current+=1;self.calls+=1;self.peak=max(self.peak,self.current)
            if self.current==2:self.two_active.set()
        try:
            if self.gate and not self.gate.wait(3):raise ValueError('test gate timeout')
            return {'ok':True,'length':len(request['publicFixture'])}
        finally:
            with self.lock:self.current-=1


class AdmissionTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path=str(Path(self.temp.name)/'service.sock')
        self.server=LimitedServer(self.path,Handler)
        self.server.compute=PublicFixtureCompute()
        self.thread=threading.Thread(target=self.server.serve_forever,daemon=True)
        self.thread.start()
        self.addCleanup(self.stop)
    def stop(self):
        self.server.shutdown();self.server.server_close();self.thread.join(timeout=5)
        self.assertFalse(self.thread.is_alive())
    def request(self,length=1<<20):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(5);client.connect(self.path)
            send(client,{'publicFixture':'x'*length})
            return receive(client,4096)

    def test_full_pool_preserves_large_frame_until_capacity_is_available(self):
        self.server.slots.acquire();self.server.slots.acquire();held=2
        pool=concurrent.futures.ThreadPoolExecutor(1)
        try:
            future=pool.submit(self.request)
            self.assertTrue(self.server.entered.wait(2))
            time.sleep(.05)
            self.assertFalse(future.done())
            self.server.slots.release();held-=1
            self.assertEqual(future.result(timeout=3),{'ok':True,'length':1<<20})
            self.assertEqual(self.server.compute.calls,1)
        finally:
            for _ in range(held):self.server.slots.release()
            pool.shutdown(wait=True)

    def test_burst_stays_bounded_without_resetting_waiting_clients(self):
        gate=threading.Event();compute=PublicFixtureCompute(gate)
        self.server.compute=compute
        with concurrent.futures.ThreadPoolExecutor(8) as pool:
            futures=[pool.submit(self.request,256<<10) for _ in range(8)]
            try:
                self.assertTrue(compute.two_active.wait(2))
                self.assertEqual(compute.peak,2)
            finally:gate.set()
            for future in futures:self.assertEqual(future.result(timeout=4),{'ok':True,'length':256<<10})
        self.assertEqual(compute.calls,8);self.assertEqual(compute.peak,2)
        self.assertFalse(compute.retired)

    def test_capacity_is_not_shared_between_service_instances(self):
        self.server.slots.acquire();self.server.slots.acquire()
        other=LimitedServer(str(Path(self.temp.name)/'second.sock'),Handler)
        try:
            self.assertTrue(other.slots.acquire(blocking=False))
            self.assertTrue(other.slots.acquire(blocking=False))
            self.assertFalse(other.slots.acquire(blocking=False))
        finally:
            other.server_close();self.server.slots.release();self.server.slots.release()

    def test_idle_client_does_not_retire_service(self):
        self.server.frame_timeout = .1
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2)
            client.connect(self.path)
            self.assertEqual(receive(client, 4096),
                             {'ok': False, 'error': 'invalid request frame'})
        self.assertFalse(self.server.compute.retired)
        self.assertEqual(self.server.compute.calls, 0)
        self.assertEqual(self.request(10), {'ok': True, 'length': 10})

    def test_trickled_frame_has_total_deadline(self):
        import struct
        self.server.frame_timeout = .15
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2)
            client.connect(self.path)
            client.sendall(struct.pack('!I', 100))
            started = time.monotonic()
            for _ in range(6):
                time.sleep(.04)
                try:
                    client.sendall(b' ')
                except OSError:
                    break
            self.assertEqual(receive(client, 4096),
                             {'ok': False, 'error': 'invalid request frame'})
            self.assertLess(time.monotonic() - started, .6)
        self.assertFalse(self.server.compute.retired)
        self.assertEqual(self.server.compute.calls, 0)
        self.assertEqual(self.request(10), {'ok': True, 'length': 10})

    def test_unauthorized_uid_never_executes_a_queued_request(self):
        self.server.compute.config['clientUids']=[]
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(2);client.connect(self.path)
            self.assertEqual(client.recv(1),b'')
        self.assertEqual(self.server.compute.calls,0)


if __name__=='__main__':unittest.main()
