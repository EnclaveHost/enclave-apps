# Minimal SOCKS5 front (user/pass, CONNECT only) mirroring the enclave egress
# front's wire shape, for exercising ENCLAVE_EGRESS locally. Logs each CONNECT.
import asyncio, socket, struct, sys
USER, PASS = b"dep1", b"tok1"
async def pipe(r, w):
    try:
        while (b := await r.read(65536)):
            w.write(b); await w.drain()
    except Exception: pass
    finally:
        try: w.close()
        except Exception: pass
async def handle(r, w):
    try:
        v, n = await r.readexactly(2); m = await r.readexactly(n)
        if 2 not in m: w.write(b"\x05\xff"); return
        w.write(b"\x05\x02"); await w.drain()
        await r.readexactly(1); ul = (await r.readexactly(1))[0]; u = await r.readexactly(ul)
        pl = (await r.readexactly(1))[0]; p = await r.readexactly(pl)
        if (u, p) != (USER, PASS): w.write(b"\x01\x01"); return
        w.write(b"\x01\x00"); await w.drain()
        _, cmd, _, at = await r.readexactly(4)
        if at == 1: host = socket.inet_ntoa(await r.readexactly(4))
        elif at == 4: host = socket.inet_ntop(socket.AF_INET6, await r.readexactly(16))
        else: host = (await r.readexactly((await r.readexactly(1))[0])).decode()
        port = struct.unpack(">H", await r.readexactly(2))[0]
        try: ur, uw = await asyncio.wait_for(asyncio.open_connection(host, port), 10)
        except Exception as e:
            print("REFUSED", host, port, e, flush=True); w.write(b"\x05\x05\x00\x01\0\0\0\0\0\0"); return
        print("CONNECT", "name" if at == 3 else "ip", host, port, flush=True)
        w.write(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00"); await w.drain()
        await asyncio.gather(pipe(r, uw), pipe(ur, w))
    except Exception as e: print("ERR", e, flush=True)
async def main():
    s = await asyncio.start_server(handle, "127.0.0.1", int(sys.argv[1]))
    async with s: await s.serve_forever()
asyncio.run(main())
