#!/usr/bin/env python3
"""Downloads ISO/IEC 14496-26 AAC conformance streams and their reference PCM from ffmpeg's FATE
suite, used by `tests/aac_conformance.rs`.

Usage: scripts/fetch_aac_vectors.py [DIR]   (default: target/aac-vectors)
Then:  AAC_VECTORS_DIR=DIR cargo test --test aac_conformance
Files already present are kept, so CI can cache DIR.
"""
import os, sys, urllib.request

BASE = "https://fate-suite.ffmpeg.org/aac/"
FILES = [
    # HE-AAC v1 (SBR), stereo 48 kHz: stream + reference PCM (16-bit little-endian).
    "al_sbr_cm_48_2.mp4", "al_sbr_hq_cm_48_2.s16",
    # HE-AAC v2 (SBR + Parametric Stereo).
    "al_sbr_ps_04_new.mp4", "al_sbr_ps_04_ur.s16",
]

def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "target/aac-vectors"
    os.makedirs(out, exist_ok=True)
    for name in FILES:
        path = os.path.join(out, name)
        if os.path.exists(path):
            continue
        req = urllib.request.Request(BASE + name, headers={"User-Agent": "alhazen-core-tests"})
        with urllib.request.urlopen(req, timeout=60) as r, open(path + ".part", "wb") as f:
            f.write(r.read())
        os.replace(path + ".part", path)
        print("fetched", name)

if __name__ == "__main__":
    main()
