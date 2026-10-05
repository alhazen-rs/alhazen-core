#!/usr/bin/env python3
"""Downloads the libvpx VP9 conformance vectors used by `video-core/tests/vp9_conformance.rs`.

Usage: scripts/fetch_vp9_vectors.py [DIR]   (default: target/vp9-vectors)
Then:  VP9_VECTORS_DIR=DIR cargo test -p video-core --test vp9_conformance
Files already present are kept, so CI can cache DIR.
"""
import os, sys, urllib.request

BASE = "https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx/"
VECTORS = [
    # Tile layouts (columns are what vp9-mt threads; rows exercise the above-context handoff).
    "vp90-2-08-tile-4x4.webm", "vp90-2-08-tile-4x1.webm", "vp90-2-08-tile_1x2.webm",
    "vp90-2-08-tile_1x4.webm", "vp90-2-08-tile_1x2_frame_parallel.webm",
    "vp90-2-08-tile_1x4_frame_parallel.webm", "vp90-2-08-tile_1x8_frame_parallel.webm",
    # Bit depths and subsamplings (profiles 1-3).
    "vp91-2-04-yuv444.webm", "vp91-2-04-yuv422.webm", "vp91-2-04-yuv440.webm",
    "vp92-2-20-10bit-yuv420.webm", "vp92-2-20-12bit-yuv420.webm",
    "vp93-2-20-10bit-yuv422.webm", "vp93-2-20-12bit-yuv444.webm",
    # Odd sizes (partial superblocks at the right/bottom edge), loop filter, frame types.
    "vp90-2-02-size-08x08.webm", "vp90-2-02-size-66x66.webm", "vp90-2-02-size-130x132.webm",
    "vp90-2-09-lf_deltas.webm", "vp90-2-16-intra-only.webm", "vp90-2-10-show-existing-frame.webm",
    "vp90-2-21-resize_inter_320x180_5_1-2.webm",
    "vp90-2-13-largescaling.webm",
]

def main():
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.join("target", "vp9-vectors")
    os.makedirs(out, exist_ok=True)
    for name in VECTORS:
        for f in (name, name + ".md5"):
            path = os.path.join(out, f)
            if os.path.exists(path):
                continue
            urllib.request.urlretrieve(BASE + f, path + ".part")
            os.replace(path + ".part", path)
    print(f"{len(VECTORS)} vectors in {out}")

if __name__ == "__main__":
    main()
