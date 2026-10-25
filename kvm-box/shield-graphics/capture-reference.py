#!/usr/bin/env python3
"""Capture the controlled 640x480, depth-24 Xvfb test display, refusing a blank frame."""
import ctypes as c
import json
from pathlib import Path
import struct
import sys
import zlib

x = c.CDLL('libX11.so.6')
def bind(name, result, *args):
    f = getattr(x, name)
    f.restype, f.argtypes = result, list(args)
    return f

open_display = bind('XOpenDisplay', c.c_void_p, c.c_char_p)
root_window = bind('XDefaultRootWindow', c.c_ulong, c.c_void_p)
get_image = bind('XGetImage', c.c_void_p, c.c_void_p, c.c_ulong, c.c_int, c.c_int,
                 c.c_uint, c.c_uint, c.c_ulong, c.c_int)
get_pixel = bind('XGetPixel', c.c_ulong, c.c_void_p, c.c_int, c.c_int)
destroy_image = bind('XDestroyImage', c.c_int, c.c_void_p)
close_display = bind('XCloseDisplay', c.c_int, c.c_void_p)

def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

display = open_display(None)
if not display:
    raise RuntimeError('cannot open the isolated test display')
image = None
try:
    width, height = 640, 480
    image = get_image(display, root_window(display), 0, 0, width, height, c.c_ulong(-1).value, 2)
    if not image:
        raise RuntimeError('XGetImage failed')
    colors, nonblack, pixels = set(), 0, bytearray()
    for y in range(height):
        pixels.append(0)  # PNG row filter: none
        for xx in range(width):
            pixel = get_pixel(image, xx, y) & 0xffffff
            colors.add(pixel)
            nonblack += pixel != 0
            pixels.extend(((pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255))
    if len(colors) < 16 or nonblack < 5000:
        raise RuntimeError(f'blank or insufficiently varied rendered frame: {len(colors)} colors, {nonblack} nonblack pixels')
    output = Path(sys.argv[1])
    output.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0))
                       + chunk(b'IDAT', zlib.compress(pixels)) + chunk(b'IEND', b''))
    output.with_suffix('.json').write_text(json.dumps({'width': width, 'height': height,
        'uniqueColors': len(colors), 'nonblackPixels': nonblack, 'execution': 'CPU reference'}, indent=2) + '\n')
finally:
    if image:
        destroy_image(image)
    close_display(display)
