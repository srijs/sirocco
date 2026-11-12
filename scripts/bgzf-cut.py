"""Writes the contents of N whole BGZF blocks cut from a piece of a BGZF file.

    python3 scripts/bgzf-cut.py WINDOW N OUT

WINDOW is a range of bytes downloaded from the middle of a BGZF file (BAM,
or a bgzip-compressed VCF), so it starts partway through a block. This finds
the first offset where three blocks chain (each header's BSIZE lands on the
next header), then decompresses N non-empty blocks from there, checks each
against its CRC-32 and ISIZE, and writes their contents back to back to OUT.
"""
import struct
import sys
import zlib


def bsize(buf, p):
    """Total length of the BGZF block at p, or None if p is not one."""
    if buf[p:p + 4] != b"\x1f\x8b\x08\x04" or p + 18 > len(buf):
        return None
    xlen = struct.unpack_from("<H", buf, p + 10)[0]
    q, end = p + 12, p + 12 + xlen
    while q + 4 <= end:
        si1, si2, slen = buf[q], buf[q + 1], struct.unpack_from("<H", buf, q + 2)[0]
        if si1 == 66 and si2 == 67 and slen == 2:
            return struct.unpack_from("<H", buf, q + 4)[0] + 1
        q += 4 + slen
    return None


def chains(buf, p):
    for _ in range(3):
        s = bsize(buf, p)
        if s is None:
            return False
        p += s
    return True


def main():
    window, n, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    buf = open(window, "rb").read()
    start = next((p for p in range(len(buf)) if chains(buf, p)), None)
    if start is None:
        sys.exit(f"{window}: no BGZF blocks found")

    raw, p = [], start
    while len(raw) < n:
        s = bsize(buf, p)
        if s is None or p + s > len(buf):
            sys.exit(f"{window}: only {len(raw)} whole blocks after offset {start}")
        crc, isize = struct.unpack_from("<II", buf, p + s - 8)
        data = zlib.decompress(buf[p:p + s], wbits=31)
        if zlib.crc32(data) != crc or len(data) != isize:
            sys.exit(f"{window}: block at {p} fails its CRC-32 or ISIZE")
        if data:
            raw.append(data)
        p += s
    open(out, "wb").write(b"".join(raw))


main()
