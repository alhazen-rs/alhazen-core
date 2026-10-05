#!/usr/bin/env python3
"""Rewrites a single-track audio WebM so every 3 consecutive blocks (SimpleBlocks, or the Block of
a BlockGroup, which is what ffmpeg writes for Vorbis) become one Xiph-laced SimpleBlock, the way mkvmerge laces Vorbis. Drops SeekHead/Cues/Tags/Void (offsets would be stale),
writes an unknown-size Segment, and strips DefaultDuration so laced frames share one timestamp.

    python3 make_laced.py vorbis_only.webm laced_vorbis.webm
"""
import sys

SEGMENT, INFO, TRACKS, TRACK_ENTRY, CLUSTER = 0x18538067, 0x1549A966, 0x1654AE6B, 0xAE, 0x1F43B675
TIMESTAMP, SIMPLE_BLOCK, BLOCK_GROUP, BLOCK, DEFAULT_DURATION = 0xE7, 0xA3, 0xA0, 0xA1, 0x23E383
GROUP = 3


def read_vint(b, i, keep_marker):
    first = b[i]
    n = 8 - first.bit_length() + 1
    v = first if keep_marker else first & (0xFF >> n)
    for k in range(1, n):
        v = (v << 8) | b[i + k]
    return v, n


def elements(b, i, end):
    """Yields (id, data) for the elements in b[i:end]."""
    while i < end:
        eid, n = read_vint(b, i, True)
        i += n
        size, m = read_vint(b, i, False)
        i += m
        if size == (1 << (7 * m)) - 1:
            size = end - i
        yield eid, b[i:i + size]
        i += size


def enc_id(eid):
    return eid.to_bytes((eid.bit_length() + 7) // 8, "big")


def enc_size(n):
    for length in range(1, 9):
        if n < (1 << (7 * length)) - 1:
            return ((1 << (7 * length)) | n).to_bytes(length, "big")
    raise ValueError(n)


def element(eid, data):
    return enc_id(eid) + enc_size(len(data)) + data


def xiph_sizes(sizes):
    out = bytearray()
    for s in sizes:
        out += b"\xff" * (s // 255) + bytes([s % 255])
    return bytes(out)


def lace(blocks):
    """blocks: SimpleBlock payloads (track vint, i16 timestamp, flags, frame)."""
    head = blocks[0][:3]  # track 1 as 0x81 + relative timestamp of the first frame
    frames = [b[4:] for b in blocks]
    flags = 0x80 | 0x02  # keyframe, Xiph lacing
    body = bytes([len(frames) - 1]) + xiph_sizes([len(f) for f in frames[:-1]]) + b"".join(frames)
    return head + bytes([flags]) + body


def main(src, dst):
    b = open(src, "rb").read()
    out = bytearray()
    (ebml_id, ebml), (seg_id, seg) = list(elements(b, 0, len(b)))[:2]
    assert seg_id == SEGMENT
    out += element(ebml_id, ebml)
    body = bytearray()
    for eid, data in elements(seg, 0, len(seg)):
        if eid == INFO:
            body += element(eid, data)
        elif eid == TRACKS:
            entries = b"".join(
                element(TRACK_ENTRY, b"".join(element(c, d) for c, d in elements(e, 0, len(e)) if c != DEFAULT_DURATION))
                for _, e in elements(data, 0, len(data))
            )
            body += element(TRACKS, entries)
        elif eid == CLUSTER:
            children, pending = bytearray(), []
            for cid, cdata in elements(data, 0, len(data)):
                if cid == BLOCK_GROUP:
                    # Same payload layout as a SimpleBlock; BlockDuration etc. are dropped.
                    cid, cdata = SIMPLE_BLOCK, next(d for c, d in elements(cdata, 0, len(cdata)) if c == BLOCK)
                if cid == SIMPLE_BLOCK:
                    pending.append(cdata)
                    if len(pending) == GROUP:
                        children += element(SIMPLE_BLOCK, lace(pending))
                        pending = []
                else:
                    children += element(cid, cdata)
            for leftover in pending:
                children += element(SIMPLE_BLOCK, leftover)
            body += element(CLUSTER, bytes(children))
    out += enc_id(SEGMENT) + b"\x01\xff\xff\xff\xff\xff\xff\xff" + body  # unknown-size Segment
    open(dst, "wb").write(out)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
