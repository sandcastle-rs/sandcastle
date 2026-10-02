#!/usr/bin/env python3
"""Write the non-zero 64 KiB chunks of a sparse disk image to stdout.

Format: b"SCX1", u64 LE size, then records of (u64 LE offset, u32 LE length,
bytes). sandcastle expands it with holes for everything not listed.
"""
import errno
import os
import struct
import sys

CHUNK = 1 << 16

def main(path: str) -> None:
    out = sys.stdout.buffer
    fd = os.open(path, os.O_RDONLY)
    size = os.fstat(fd).st_size
    out.write(b"SCX1" + struct.pack("<Q", size))
    pos = 0
    while pos < size:
        try:
            start = os.lseek(fd, pos, os.SEEK_DATA)
        except OSError as e:
            if e.errno == errno.ENXIO:  # no data after pos
                break
            raise
        end = os.lseek(fd, start, os.SEEK_HOLE)
        off = start
        while off < end:
            n = min(CHUNK, end - off)
            buf = os.pread(fd, n, off)
            if buf.count(0) != len(buf):
                out.write(struct.pack("<QI", off, len(buf)))
                out.write(buf)
            off += n
        pos = end

if __name__ == "__main__":
    main(sys.argv[1])
